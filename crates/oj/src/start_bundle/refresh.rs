// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! React Fast Refresh in the dev client bundle, as @vitejs/plugin-react sets
//! it up for Vite's bundled dev: the refresh preamble runs first in the entry
//! (before react-dom loads), oxc's refresh transform instruments the app's
//! modules (rolldown's `builtin:vite-transform` with `jsx.refresh`), and the
//! wrapper registers each module's exports and accepts the update when they
//! are all components (`builtin:vite-react-refresh-wrapper`, ported from
//! rolldown 1.2.14, MIT). The runtime is oj's copy of plugin-react's.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use rolldown_common::ModuleType;
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, HookTransformArgs, HookTransformOutput, HookTransformOutputMap,
    HookTransformReturn, Plugin, PluginContext, PluginHookMeta, PluginOrder,
    SharedLoadPluginContext, SharedTransformPluginContext,
};

const RUNTIME_PUBLIC_PATH: &str = "/@react-refresh";
const PREAMBLE: &str = "\0oj-react-refresh-preamble";

/// The preamble and the runtime's public path, ahead of plugin-react's own
/// pre-ordered resolveId (which would claim the id for a load oj never runs).
#[derive(Debug)]
pub(crate) struct RefreshPreamble {
    /// oj's copy of plugin-react's refresh runtime.
    pub runtime: PathBuf,
    /// The client entry, which imports the preamble first.
    pub entry: PathBuf,
}

impl Plugin for RefreshPreamble {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:react-refresh-preamble")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        use rolldown_plugin::HookUsage;
        HookUsage::ResolveId | HookUsage::Load | HookUsage::Transform
    }

    fn resolve_id_meta(&self) -> Option<PluginHookMeta> {
        Some(PluginHookMeta {
            order: Some(PluginOrder::Pre),
        })
    }

    async fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        Ok(match args.specifier {
            RUNTIME_PUBLIC_PATH => Some(HookResolveIdOutput::from_id(
                self.runtime.to_string_lossy().into_owned(),
            )),
            PREAMBLE => Some(HookResolveIdOutput::from_id(PREAMBLE)),
            _ => None,
        })
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        if args.id != PREAMBLE {
            return Ok(None);
        }
        Ok(Some(HookLoadOutput {
            code: arcstr::literal!(
                "import { injectIntoGlobalHook } from \"/@react-refresh\";\n\
                 injectIntoGlobalHook(window);\n\
                 window.$RefreshReg$ = () => {};\n\
                 window.$RefreshSig$ = () => (type) => type;"
            ),
            module_type: Some(ModuleType::Js),
            ..Default::default()
        }))
    }

    async fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> HookTransformReturn {
        if Path::new(args.id) != self.entry {
            return Ok(None);
        }
        Ok(Some(HookTransformOutput {
            code: Some(format!(
                "import {}; {}",
                serde_json::Value::String(PREAMBLE.into()),
                args.code
            )),
            ..Default::default()
        }))
    }
}

/// oxc's TypeScript strip and JSX transform with refresh registration, on the
/// app's TypeScript and JSX modules (Vite's oxc plugin: `.ts`, `.mts`,
/// `.jsx`, `.tsx`, never `.js` or a dependency).
#[derive(Debug)]
pub(crate) struct RefreshTransform {
    pub jsx: oj_compiler::JsxConfig,
}

fn clean(id: &str) -> &str {
    id.split(['?', '#']).next().unwrap_or(id)
}

impl Plugin for RefreshTransform {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:react-refresh-transform")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::Transform
    }

    async fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> HookTransformReturn {
        let id = args.id;
        let path = clean(id);
        if id.contains('\0')
            || path.contains("/node_modules/")
            || !matches!(
                Path::new(path).extension().and_then(|e| e.to_str()),
                Some("ts" | "mts" | "jsx" | "tsx")
            )
        {
            return Ok(None);
        }
        let (code, map) =
            oj_compiler::transform_for_bundle(Path::new(path), args.code, &self.jsx, true)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Some(HookTransformOutput {
            code: Some(code),
            map: match map.and_then(|m| {
                oxc_sourcemap::SourceMap::from_json_string(&m)
                    .ok()
                    .map(|s| s.into_owned())
            }) {
                Some(map) => HookTransformOutputMap::Sourcemap(Box::new(map)),
                None => HookTransformOutputMap::Omitted,
            },
            module_type: Some(ModuleType::Js),
            ..Default::default()
        }))
    }
}

/// plugin-react's refresh boundary: a module whose code registered
/// components (or that only declares class components) registers its exports
/// with the runtime and accepts its own update when the runtime says they are
/// all components.
#[derive(Debug)]
pub(crate) struct RefreshWrapper {
    jsx_runtime: String,
    jsx_dev_runtime: String,
}

impl RefreshWrapper {
    pub fn new(jsx_import_source: &str) -> Self {
        Self {
            jsx_runtime: format!("{jsx_import_source}/jsx-runtime"),
            jsx_dev_runtime: format!("{jsx_import_source}/jsx-dev-runtime"),
        }
    }

    fn wrap(code: &str, id: &str) -> Option<String> {
        static REACT_COMPONENT: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| {
                regex::Regex::new(r"extends\s+(?:React\.)?(?:Pure)?Component").unwrap()
            });
        let has_refresh = code.contains("$RefreshReg$(");
        if !has_refresh && !REACT_COMPONENT.is_match(code) {
            return None;
        }
        let id = serde_json::Value::String(id.to_string()).to_string();
        let mut out = code.to_string();
        out.push_str(&format!(
            r#"
import * as RefreshRuntime from "{RUNTIME_PUBLIC_PATH}";
const inWebWorker = typeof WorkerGlobalScope !== 'undefined' && self instanceof WorkerGlobalScope;
import * as __vite_react_currentExports from {id};
if (import.meta.hot && !inWebWorker) {{
  if (!window.$RefreshReg$) {{
    throw new Error(
      "@vitejs/plugin-react can't detect preamble. Something is wrong."
    );
  }}

  const currentExports = __vite_react_currentExports;
  queueMicrotask(() => {{
    RefreshRuntime.registerExportsForReactRefresh({id}, currentExports);
    import.meta.hot.accept((nextExports) => {{
      if (!nextExports) return;
      const invalidateMessage = RefreshRuntime.validateRefreshBoundaryAndEnqueueUpdate({id}, currentExports, nextExports);
      if (invalidateMessage) import.meta.hot.invalidate(invalidateMessage);
    }});
  }});
}}
"#
        ));
        if has_refresh {
            out.push_str(&format!(
                "function $RefreshReg$(type, id) {{ return RefreshRuntime.register(type, {id} + ' ' + id); }}\n\
                 function $RefreshSig$() {{ return RefreshRuntime.createSignatureFunctionForTransform(); }}\n"
            ));
        }
        Some(out)
    }
}

impl Plugin for RefreshWrapper {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:react-refresh-wrapper")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::Transform
    }

    async fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> HookTransformReturn {
        let id = args.id;
        let path = clean(id);
        if id.starts_with('\0')
            || path.contains("/node_modules/")
            || !matches!(
                Path::new(path).extension().and_then(|e| e.to_str()),
                Some("js" | "jsx" | "ts" | "tsx")
            )
        {
            return Ok(None);
        }
        let uses_jsx = path.ends_with('x')
            || args.code.contains(&self.jsx_dev_runtime)
            || args.code.contains(&self.jsx_runtime);
        if !uses_jsx {
            return Ok(None);
        }
        Ok(Self::wrap(args.code, id).map(|code| HookTransformOutput {
            code: Some(code),
            map: HookTransformOutputMap::Null,
            ..Default::default()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_get_the_refresh_boundary() {
        let code = "export function App() {}\n$RefreshReg$(_c, \"App\");\n";
        let out = RefreshWrapper::wrap(code, "/app/src/App.tsx").unwrap();
        assert!(out.starts_with(code));
        assert!(out.contains("import * as RefreshRuntime from \"/@react-refresh\";"));
        assert!(out.contains(
            "RefreshRuntime.registerExportsForReactRefresh(\"/app/src/App.tsx\", currentExports);"
        ));
        assert!(out.contains("function $RefreshReg$(type, id) { return RefreshRuntime.register(type, \"/app/src/App.tsx\" + ' ' + id); }"));
        let class_only = "class Card extends React.PureComponent {}";
        let out = RefreshWrapper::wrap(class_only, "/app/src/Card.jsx").unwrap();
        assert!(!out.contains("function $RefreshReg$"));
        assert!(RefreshWrapper::wrap("export const x = 1;", "/app/src/x.tsx").is_none());
    }

    #[test]
    fn the_refresh_transform_registers_components() {
        let (code, map) = oj_compiler::transform_for_bundle(
            Path::new("/app/src/App.tsx"),
            "export function App(): JSX.Element { return <div />; }",
            &oj_compiler::JsxConfig::default(),
            true,
        )
        .unwrap();
        assert!(code.contains("$RefreshReg$("), "{code}");
        assert!(code.contains("jsx-dev-runtime"), "{code}");
        assert!(!code.contains(": JSX.Element"), "{code}");
        assert!(map.is_some());
    }
}
