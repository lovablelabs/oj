// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's verdict on a module's side effects for the imports rolldown's own
//! resolver answers (the plugin host passed on them, or there is none): the
//! package's `sideEffects` field, as `moduleSideEffects`. rolldown takes a
//! `true` as "analyze the module" and a `false` as none, so a pure module a
//! glob covers (a StyleX tokens file under `**/*.stylex.ts`) still drops out
//! of a chunk that imports it for nothing, as under Vite's own resolver.
//! Left to rolldown, the field is read as the verdict itself, and every
//! chunk kept an `import "./conditions.stylex-*.js"` (Lovable's server
//! bundle: 64 of them).
//!
//! And Vite's answer for a bare import nothing resolves: when the importing
//! package lists it as an optional peer dependency, the module
//! `__vite-optional-peer-dep:<import>:<package>`, which throws when it runs
//! (framer-motion's `@emotion/is-prop-valid`); rolldown left a `require()`
//! of it to its runtime's require shim.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use rolldown_common::side_effects::HookSideEffects;
use rolldown_common::ModuleType;
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, Plugin, PluginContext, PluginContextResolveOptions,
    SharedLoadPluginContext,
};

const OPTIONAL_PEER_DEP: &str = "__vite-optional-peer-dep";

/// Last in a bundle's plugins: what reaches it, no plugin answered.
#[derive(Debug)]
pub(crate) struct ViteSideEffects {
    /// The app's root: an optional peer dependency is a package's, never the
    /// app's own import (Vite's `basedir !== root`).
    pub root: PathBuf,
    /// A production build: the throwing module asks "Is it installed?".
    pub is_production: bool,
}

impl Plugin for ViteSideEffects {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:vite-side-effects")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::ResolveId | rolldown_plugin::HookUsage::Load
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        let Some(rest) = args
            .id
            .strip_prefix(OPTIONAL_PEER_DEP)
            .and_then(|r| r.strip_prefix(':'))
        else {
            return Ok(None);
        };
        let Some((peer, parent)) = rest.rsplit_once(':') else {
            return Ok(None);
        };
        // The module rolldown's native vite:resolve (a build's) loads for it,
        // word for word: an ES module, so the importing chunk wraps it as
        // Vite's does (`__esm`, a `default` export), with the question only
        // a production build asks.
        let code = format!(
            "export default {{}};\nconst peerDep = {peer};\nconst parentDep = {parent};\nthrow new Error(`Could not resolve \"${{peerDep}}\" imported by \"${{parentDep}}\".{question}`)",
            peer = serde_json::Value::String(peer.to_string()),
            parent = serde_json::Value::String(parent.to_string()),
            question = if self.is_production {
                " Is it installed?"
            } else {
                ""
            },
        );
        Ok(Some(HookLoadOutput {
            code: code.into(),
            module_type: Some(ModuleType::Js),
            ..Default::default()
        }))
    }

    async fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        let found = match ctx
            .resolve(
                args.specifier,
                args.importer,
                Some(PluginContextResolveOptions {
                    import_kind: args.kind,
                    is_entry: args.is_entry,
                    skip_self: true,
                    custom: std::sync::Arc::clone(&args.custom),
                    attributes: args.attributes.clone(),
                }),
            )
            .await?
        {
            Ok(found) => found,
            Err(_) => {
                return Ok(optional_peer_dep(args.specifier, args.importer, &self.root)
                    .map(HookResolveIdOutput::from_id));
            }
        };
        if found.external.is_external() || found.side_effects.is_some() {
            return Ok(None);
        }
        let Some(verdict) = found.package_json.as_ref().and_then(|p| {
            let relative = Path::new(found.id.as_str())
                .strip_prefix(p.realpath().parent()?)
                .unwrap_or(Path::new(found.id.as_str()));
            p.check_side_effects_for(&relative.to_string_lossy())
        }) else {
            return Ok(None);
        };
        let mut out = HookResolveIdOutput::from_resolved_id(found);
        out.side_effects = Some(if verdict {
            HookSideEffects::True
        } else {
            HookSideEffects::False
        });
        Ok(Some(out))
    }
}

/// Vite's id for a bare import nothing resolves that the importing package
/// declares as an optional peer dependency (`peerDependencies` and
/// `peerDependenciesMeta[name].optional` in the nearest package.json above
/// the importer, outside the app's root): `__vite-optional-peer-dep:<import>:<package>`.
pub(crate) fn optional_peer_dep(
    specifier: &str,
    importer: Option<&str>,
    root: &Path,
) -> Option<String> {
    if specifier.starts_with('.')
        || specifier.starts_with('/')
        || specifier.starts_with("node:")
        || specifier.contains('\0')
    {
        return None;
    }
    let name = npm_package_name(specifier)?;
    let importer = Path::new(importer?.split('?').next()?);
    let dir = importer.parent()?;
    if dir == root {
        return None;
    }
    let mut at = dir;
    loop {
        let candidate = at.join("package.json");
        if candidate.is_file() {
            let json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&candidate).ok()?).ok()?;
            let optional = json.get("peerDependencies")?.get(name).is_some()
                && json
                    .get("peerDependenciesMeta")?
                    .get(name)?
                    .get("optional")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
            if !optional {
                return None;
            }
            let parent = json.get("name")?.as_str()?;
            return Some(format!("{OPTIONAL_PEER_DEP}:{specifier}:{parent}"));
        }
        at = at.parent()?;
    }
}

/// The package a bare specifier names (`@scope/name/sub` is `@scope/name`).
fn npm_package_name(specifier: &str) -> Option<&str> {
    let mut parts = specifier.splitn(3, '/');
    let first = parts.next()?;
    if first.starts_with('@') {
        let second = parts.next()?;
        return Some(&specifier[..first.len() + 1 + second.len()]);
    }
    Some(first)
}

#[cfg(test)]
mod tests {
    use super::{npm_package_name, optional_peer_dep};

    #[test]
    fn an_optional_peer_dependency_of_a_package_gets_vites_id() {
        let dir = std::env::temp_dir().join(format!("oj-opt-peer-{}", std::process::id()));
        let pkg = dir.join("node_modules/motion");
        std::fs::create_dir_all(pkg.join("dist")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "motion", "peerDependencies": { "@emotion/is-prop-valid": "*", "react": "*" }, "peerDependenciesMeta": { "@emotion/is-prop-valid": { "optional": true } } }"#,
        )
        .unwrap();
        let importer = pkg.join("dist/index.mjs");
        let importer = importer.to_str().unwrap();
        assert_eq!(
            optional_peer_dep("@emotion/is-prop-valid", Some(importer), &dir),
            Some("__vite-optional-peer-dep:@emotion/is-prop-valid:motion".to_string())
        );
        assert_eq!(
            optional_peer_dep("@emotion/is-prop-valid/sub", Some(importer), &dir),
            Some("__vite-optional-peer-dep:@emotion/is-prop-valid/sub:motion".to_string())
        );
        // A peer dependency that is not optional, a relative import and the
        // app's own import (the importer at the root) are not its to answer.
        assert_eq!(optional_peer_dep("react", Some(importer), &dir), None);
        assert_eq!(optional_peer_dep("./x", Some(importer), &dir), None);
        let own = dir.join("main.js");
        assert_eq!(
            optional_peer_dep("@emotion/is-prop-valid", Some(own.to_str().unwrap()), &dir),
            None
        );
        assert_eq!(npm_package_name("@scope/name/deep"), Some("@scope/name"));
        assert_eq!(npm_package_name("name/deep"), Some("name"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
