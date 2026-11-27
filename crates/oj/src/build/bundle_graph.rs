// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Rolldown's module graph as the app's plugins read it. Under `vite build`,
//! `this.getModuleInfo` and `this.getModuleIds` are rolldown's own, so they
//! know every module the bundle has met, including the ones a hook filter
//! kept away from a plugin. A plugin host's records only hold what its own
//! hooks saw, so a bundle hands the host this graph at buildStart.

use std::path::Path;
use std::sync::Arc;

use rolldown_common::{ModuleId, ModuleInfo};
use rolldown_plugin::PluginContext;
use rolldown_utils::indexmap::FxIndexSet;

/// The text of a bundle's outputs by file name, for the hooks that see the
/// whole bundle (see [`RolldownGraph::set_output_texts`]).
pub(crate) type OutputTexts = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// One bundle's modules. Code reads as JS, as after Vite's vite:oxc, lowered
/// with the app's JSX settings.
pub(crate) struct RolldownGraph {
    ctx: PluginContext,
    jsx: oj_compiler::JsxConfig,
    /// See [`RolldownGraph::set_output_texts`].
    texts: std::sync::Mutex<Option<OutputTexts>>,
}

impl std::fmt::Debug for RolldownGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RolldownGraph")
    }
}

impl RolldownGraph {
    pub fn new(ctx: &PluginContext, jsx: oj_compiler::JsxConfig) -> Arc<Self> {
        Arc::new(Self {
            ctx: ctx.clone(),
            jsx,
            texts: std::sync::Mutex::new(None),
        })
    }

    /// The texts of the bundle a `generateBundle` or `writeBundle` hook is
    /// about to see (`crate::build::serialize_bundle_for_hooks` gives the
    /// bundle's JSON and this lookup together), and `None` once the hook
    /// returned: the host reads a chunk's code or an asset's source here
    /// when a hook asks for it, instead of taking every text in the call.
    pub fn set_output_texts(&self, texts: Option<OutputTexts>) {
        *self.texts.lock().unwrap() = texts;
    }
}

impl oj_server::plugins::BundleGraph for RolldownGraph {
    fn output_text(&self, file: &str) -> Option<String> {
        // The lookup runs outside the lock: the isolate thread asks, and a
        // hook on it may ask again before this answers.
        let texts = self.texts.lock().unwrap().clone();
        texts.and_then(|texts| texts(file))
    }

    fn module_info(&self, id: &str) -> Option<serde_json::Value> {
        self.ctx.get_module_info(id).map(|info| graph_fields(&info))
    }

    fn module_code(&self, id: &str) -> Option<String> {
        let info = self.ctx.get_module_info(id)?;
        let code = info.code.as_deref()?;
        Some(module_code_as_js(id, code, &self.jsx))
    }

    fn module_ids(&self) -> Vec<String> {
        self.ctx
            .get_module_ids()
            .into_iter()
            .map(|id| id.to_string())
            .collect()
    }
}

/// Rollup's ModuleInfo fields but `code`. Rolldown records a module as it
/// starts loading it; until it is parsed, its exports are unknown (null).
pub(crate) fn graph_fields(info: &ModuleInfo) -> serde_json::Value {
    fn ids(set: &FxIndexSet<ModuleId>) -> Vec<&str> {
        set.iter().map(|m| m.as_str()).collect()
    }
    let parsed = info.code.is_some();
    let exports: Option<Vec<&str>> =
        parsed.then(|| info.exports.iter().map(|e| e.as_str()).collect());
    serde_json::json!({
        "id": info.id.as_str(),
        "isEntry": info.is_entry,
        "importedIds": ids(&info.imported_ids),
        "dynamicallyImportedIds": ids(&info.dynamically_imported_ids),
        "importers": ids(&info.importers),
        "dynamicImporters": ids(&info.dynamic_importers),
        "hasDefaultExport": exports.as_ref().map(|e| e.contains(&"default")),
        "exports": exports,
    })
}

/// The code plugins read for a module: JS, as after Vite's vite:oxc (a
/// TypeScript or JSX module lowered; anything else, or a module that does
/// not lower, as loaded). Rolldown holds the code as loaded, since oj lowers
/// TS and JSX inside rolldown rather than in a plugin.
pub(crate) fn module_code_as_js(id: &str, code: &str, jsx: &oj_compiler::JsxConfig) -> String {
    let file = id.split('?').next().unwrap_or(id);
    let lowers = matches!(
        Path::new(file).extension().and_then(|e| e.to_str()),
        Some("ts" | "tsx" | "mts" | "cts" | "jsx")
    );
    if !lowers {
        return code.to_string();
    }
    oj_compiler::transform_for_bundle(Path::new(file), code, jsx, false)
        .map_or_else(|_| code.to_string(), |(js, _)| js)
}

/// The app's JSX settings for oj's own TS and JSX lowering.
pub(crate) fn jsx_config(config: &oj_config::OjConfig) -> oj_compiler::JsxConfig {
    let jsx = oj_config::jsx_settings(config);
    oj_compiler::JsxConfig {
        runtime: jsx.runtime,
        import_source: jsx.import_source,
        pragma: jsx.pragma,
        pragma_frag: jsx.pragma_frag,
    }
}
