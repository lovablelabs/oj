// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The server-fn resolver's imports of provider modules
//! (`<file>?tss-serverfn-split`): resolved here, ahead of the plugin host, so
//! the id keeps its query. The chain under the host (an app's `pre` resolve
//! hooks, a framework plugin's) answered some with the file alone, and the
//! provider then WAS the module: compiled as the caller, with every handler
//! replaced by a stub and none left to call. Vite's own resolver keeps the
//! query of a file id as a matter of course.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use rolldown_plugin::{
    HookResolveIdArgs, HookResolveIdOutput, HookResolveIdReturn, Plugin, PluginContext,
};

pub(crate) const PROVIDER_QUERY: &str = "tss-serverfn-split";

#[derive(Debug)]
pub(crate) struct ServerFnProvider;

impl Plugin for ServerFnProvider {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:start-server-fn-provider")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::ResolveId
    }

    async fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        let Some(id) = provider_id(args.specifier, args.importer) else {
            return Ok(None);
        };
        Ok(Some(HookResolveIdOutput::from_id(id)))
    }
}

/// The provider id a specifier names: its file, relative to the importer or
/// absolute, with the `?tss-serverfn-split` query kept. None for any other
/// specifier.
pub(crate) fn provider_id(specifier: &str, importer: Option<&str>) -> Option<String> {
    let (path, query) = specifier.split_once('?')?;
    if !query.split('&').any(|p| p == PROVIDER_QUERY) {
        return None;
    }
    let file = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else if path.starts_with("./") || path.starts_with("../") {
        let importer = importer?.split('?').next()?;
        Path::new(importer).parent()?.join(path)
    } else {
        return None;
    };
    // Lexically first: the resolver lives in the cache directory, whose `..`
    // must fold even where that directory is not on disk yet.
    let mut normalized = PathBuf::new();
    for part in file.components() {
        match part {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    let file = std::fs::canonicalize(&normalized).unwrap_or(normalized);
    if !file.is_file() {
        return None;
    }
    Some(format!("{}?{query}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::provider_id;

    #[test]
    fn a_provider_specifier_resolves_to_its_file_with_the_query_kept() {
        let dir = std::env::temp_dir().join(format!("oj-provider-id-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/fns.ts"), "export const a = 1;\n").unwrap();
        let importer = dir.join(".oj-cache/server-fn-resolver.build.mjs");
        let want = format!(
            "{}?tss-serverfn-split",
            std::fs::canonicalize(dir.join("src/fns.ts"))
                .unwrap()
                .display()
        );
        assert_eq!(
            provider_id(
                "../src/fns.ts?tss-serverfn-split",
                Some(importer.to_str().unwrap())
            ),
            Some(want.clone())
        );
        assert_eq!(
            provider_id(
                &format!("{}?tss-serverfn-split", dir.join("src/fns.ts").display()),
                None
            ),
            Some(want)
        );
        // Other queries, bare specifiers and missing files are not its to answer.
        assert_eq!(
            provider_id("../src/fns.ts?url", Some(importer.to_str().unwrap())),
            None
        );
        assert_eq!(provider_id("pkg/fns.ts?tss-serverfn-split", None), None);
        assert_eq!(
            provider_id(
                "../src/gone.ts?tss-serverfn-split",
                Some(importer.to_str().unwrap())
            ),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
