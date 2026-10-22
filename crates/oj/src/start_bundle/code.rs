// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The source rewrites a Start bundle applies to the app's code, in the
//! order Vite's plugins apply them: `new URL(..., import.meta.url)` (client
//! bundles), `import.meta.glob`, and TanStack's `createServerFn` split (the
//! RPC stub in the browser, the provider export on the server).

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use oj_compiler::glob::GlobCall;
use oj_compiler::meta_glob::{self, MetaUrlTarget};
use rolldown_plugin::{
    HookTransformArgs, HookTransformOutput, HookTransformReturn, Plugin,
    PluginContextResolveOptions, SharedTransformPluginContext,
};

/// Which bundle the code goes to.
#[derive(Debug, Clone)]
pub(crate) enum CodeTarget {
    /// The browser client: URL and glob rewrites everywhere, the server-fn
    /// stub in app code.
    Client,
    /// The server bundle: globs and the server-fn provider in app code.
    Server,
    /// A worker bundle whose entry is `entry`: URL and glob rewrites
    /// everywhere, a worker starting itself getting `self.location.href`.
    Worker { entry: PathBuf },
}

pub(crate) struct StartCodePlugin {
    root: PathBuf,
    target: CodeTarget,
    /// Every glob a client bundle expanded, for the dev rebundle gate (a file
    /// created that one matches changes the bundle).
    glob_scopes: Option<Arc<Mutex<Vec<GlobCall>>>>,
    warned: Mutex<HashSet<String>>,
}

impl std::fmt::Debug for StartCodePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StartCodePlugin")
    }
}

impl StartCodePlugin {
    pub fn new(root: &Path, target: CodeTarget) -> Self {
        Self {
            root: root.to_path_buf(),
            target,
            glob_scopes: None,
            warned: Mutex::new(HashSet::new()),
        }
    }

    pub fn recording_globs(mut self, scopes: Arc<Mutex<Vec<GlobCall>>>) -> Self {
        self.glob_scopes = Some(scopes);
        self
    }

    fn warn_once(&self, message: String) {
        if self.warned.lock().unwrap().insert(message.clone()) {
            eprintln!("{message}");
        }
    }
}

/// App source the server-fn and glob rewrites own: not a dependency, not a
/// virtual module, a script.
fn is_app_code(id: &str) -> bool {
    !id.contains("/node_modules/")
        && !id.starts_with('\0')
        && matches!(
            Path::new(id.split('?').next().unwrap_or(id))
                .extension()
                .and_then(|e| e.to_str()),
            Some("ts" | "tsx" | "js" | "jsx" | "mjs")
        )
}

fn is_script(id: &str) -> bool {
    !id.starts_with('\0')
        && matches!(
            Path::new(id.split('?').next().unwrap_or(id))
                .extension()
                .and_then(|e| e.to_str()),
            Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts")
        )
}

impl Plugin for StartCodePlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:start-code")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::Transform
    }

    async fn transform(
        &self,
        ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> HookTransformReturn {
        let id = args.id;
        if !is_script(id) {
            return Ok(None);
        }
        let app = is_app_code(id);
        let file = PathBuf::from(id.split('?').next().unwrap_or(id));
        let original = args.code.as_str();
        let mut code: Cow<str> = Cow::Borrowed(original);

        let rewrites_urls = !matches!(self.target, CodeTarget::Server);
        let mut urls_changed = false;
        if rewrites_urls && code.contains("import.meta.url") {
            let sites =
                meta_glob::scan_import_meta_urls(&code, id).map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut targets = Vec::with_capacity(sites.len());
            for site in &sites {
                let target = match site.kind.file() {
                    None => MetaUrlTarget::Unresolved,
                    Some(spec) => match self.resolve_url_file(&ctx, spec, id).await? {
                        Some(abs) => match &self.target {
                            CodeTarget::Worker { entry } if *entry == abs => {
                                MetaUrlTarget::SelfWorker
                            }
                            _ => MetaUrlTarget::File(abs),
                        },
                        None => {
                            // Vite's warning for a file that is not there; a
                            // `/` URL is a public file and a scheme no file.
                            if !spec.starts_with('/') && !meta_glob::has_scheme(spec) {
                                self.warn_once(format!(
                                    "{} doesn't exist at build time, it will remain unchanged to be resolved at runtime. If this is intended, you can use the /* @vite-ignore */ comment to suppress this warning.",
                                    &code[site.start..site.end]
                                ));
                            }
                            MetaUrlTarget::Unresolved
                        }
                    },
                };
                targets.push(target);
            }
            let out = meta_glob::apply_import_meta_urls(&code, &sites, &targets);
            if out != code {
                urls_changed = true;
                code = Cow::Owned(out);
            }
        }

        // App code always expands its globs; a dependency only the ones a URL
        // rewrite produced (Vite runs the glob plugin on every module, and a
        // dependency's own globs come prebuilt).
        if (app || urls_changed || matches!(self.target, CodeTarget::Worker { .. }))
            && code.contains("import.meta.glob")
        {
            let mut scopes = Vec::new();
            let out = meta_glob::transform_glob(&code, &file, &self.root, &mut scopes)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if let Some(record) = &self.glob_scopes {
                record.lock().unwrap().extend(scopes);
            }
            if out != code {
                code = Cow::Owned(out);
            }
        }

        if app && code.contains("createServerFn") {
            let rel = file
                .strip_prefix(&self.root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| file.to_string_lossy().into_owned());
            let rewritten = match self.target {
                CodeTarget::Client => oj_compiler::server_fns::rewrite_client(&code, &rel)
                    .map_err(|e| anyhow::anyhow!("{e}"))?,
                CodeTarget::Server => oj_compiler::server_fns::rewrite_server(&code, &rel),
                CodeTarget::Worker { .. } => None,
            };
            if let Some(out) = rewritten {
                code = Cow::Owned(out);
            }
        }

        Ok(match code {
            Cow::Borrowed(_) => None,
            Cow::Owned(out) => Some(HookTransformOutput {
                code: Some(out),
                ..Default::default()
            }),
        })
    }
}

impl StartCodePlugin {
    /// Vite's resolution of a URL's file: a relative one through the bundler
    /// from the importer; a `/` one from the project root (else a public file,
    /// left as written); any other with preferRelative, the importer's
    /// directory before a package.
    async fn resolve_url_file(
        &self,
        ctx: &SharedTransformPluginContext,
        file: &str,
        importer: &str,
    ) -> anyhow::Result<Option<PathBuf>> {
        if meta_glob::has_scheme(file) {
            return Ok(None);
        }
        if let Some(rest) = file.strip_prefix('/') {
            let abs = self.root.join(rest);
            return Ok(abs.is_file().then_some(abs));
        }
        let resolve = |spec: String| async move {
            let r = ctx
                .inner
                .resolve(
                    &spec,
                    Some(importer),
                    Some(PluginContextResolveOptions {
                        skip_self: true,
                        ..Default::default()
                    }),
                )
                .await?;
            anyhow::Ok(r.ok().and_then(|r| {
                let id = r.id.as_str();
                (!id.starts_with('\0')
                    && matches!(r.external, rolldown_common::ResolvedExternal::Bool(false)))
                .then(|| PathBuf::from(id.split(['?', '#']).next().unwrap_or(id)))
            }))
        };
        if file.starts_with('.') {
            return resolve(file.to_string()).await;
        }
        if let Some(found) = resolve(format!("./{file}")).await? {
            return Ok(Some(found));
        }
        resolve(file.to_string()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_code_is_the_apps_scripts() {
        assert!(is_app_code("/app/src/a.tsx"));
        assert!(is_app_code("/app/src/a.ts?tsr-split=x"));
        assert!(!is_app_code("/app/node_modules/x/a.js"));
        assert!(!is_app_code("\0virtual"));
        assert!(!is_app_code("/app/src/a.css"));
        assert!(is_script("/app/node_modules/x/a.cjs"));
    }
}
