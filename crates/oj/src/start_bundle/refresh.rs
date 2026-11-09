// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! React Fast Refresh in the dev client bundle, as @vitejs/plugin-react sets
//! it up for Vite's bundled dev: the refresh preamble runs before any
//! component module (and before react-dom loads), oxc's refresh transform
//! instruments the app's modules (rolldown's `builtin:vite-transform` with
//! `jsx.refresh`), and the wrapper registers each module's exports and
//! accepts the update when they are all components
//! (`builtin:vite-react-refresh-wrapper`, ported from rolldown 1.2.14, MIT).
//! The runtime is oj's copy of plugin-react's.
//!
//! The preamble is the runtime module's own tail, not a module of its own:
//! every wrapped module imports the runtime, so wherever rolldown chunks the
//! graph, ES module order evaluates the runtime (and with it the preamble)
//! before the first wrapped module's body. A separate preamble module inlined
//! into the entry ran too late: the entry chunk evaluates the shared chunks
//! it imports first, and a component shared with a lazy route lives in one
//! of those, so its `$RefreshReg$` check threw on every fresh page load
//! (Vite's unbundled dev has no such chunking; its bundled dev puts the
//! preamble in the HTML, ahead of the entry script). The entry still imports
//! the runtime first, so it also runs before react-dom's module body reads
//! the devtools hook the preamble installs.
//!
//! This holds for rolldown's own chunking (a chunk holding the runtime never
//! imports one holding a wrapped module: the runtime is reached from every
//! entry that reaches a wrapped module). An app's manual chunk group that
//! captures the runtime together with an importer of a wrapped module it
//! leaves out would make such a cycle; should that bite, the Vite-parity
//! fallback is a second input for the preamble and a `<script type="module">`
//! for it ahead of the entry script.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use rolldown_common::ModuleType;
use rolldown_plugin::{
    HookResolveIdArgs, HookResolveIdOutput, HookResolveIdReturn, HookTransformArgs,
    HookTransformOutput, HookTransformOutputMap, HookTransformReturn, Plugin, PluginContext,
    PluginHookMeta, PluginOrder, SharedTransformPluginContext,
};

const RUNTIME_PUBLIC_PATH: &str = "/@react-refresh";

/// plugin-react's preamble, run as the runtime module's last statements
/// (`injectIntoGlobalHook` is the module's own export). Guarded for a bundle
/// evaluated off the main thread, where the wrapper skips refresh too.
const PREAMBLE_TAIL: &str = r#"
if (typeof window !== "undefined") {
  injectIntoGlobalHook(window);
  window.$RefreshReg$ = () => {};
  window.$RefreshSig$ = () => (type) => type;
}
"#;

/// The entry's first import, so the runtime (and its preamble) has the
/// lowest execution order in the graph: ahead of react-dom. On the entry's
/// first line, so its line numbers stand without a map.
fn entry_code(code: &str) -> String {
    format!("import {RUNTIME_PUBLIC_PATH:?}; {code}")
}

/// The runtime module, installing the preamble as it evaluates.
fn runtime_code(code: &str) -> String {
    format!("{code}{PREAMBLE_TAIL}")
}

/// The runtime's public path, ahead of plugin-react's own pre-ordered
/// resolveId (which would claim the id for a load oj never runs), and the
/// preamble: the entry's first import and the runtime's tail.
#[derive(Debug)]
pub(crate) struct RefreshPreamble {
    /// oj's copy of plugin-react's refresh runtime.
    pub runtime: PathBuf,
    /// The client entry, which imports the runtime first.
    pub entry: PathBuf,
}

impl Plugin for RefreshPreamble {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:react-refresh-preamble")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        use rolldown_plugin::HookUsage;
        HookUsage::ResolveId | HookUsage::Transform
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
            _ => None,
        })
    }

    async fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> HookTransformReturn {
        let path = Path::new(args.id);
        let code = if path == self.entry {
            entry_code(args.code)
        } else if path == self.runtime {
            runtime_code(args.code)
        } else {
            return Ok(None);
        };
        Ok(Some(HookTransformOutput {
            code: Some(code),
            map: HookTransformOutputMap::Null,
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

    #[test]
    fn the_entry_imports_the_runtime_first() {
        let out = entry_code("import { hydrateRoot } from \"react-dom/client\";\n");
        // On the entry's own first line: its line numbers stand.
        assert!(
            out.starts_with("import \"/@react-refresh\"; import { hydrateRoot }"),
            "{out}"
        );
        assert_eq!(out.lines().count(), 1);
    }

    #[test]
    fn the_runtime_installs_the_preamble_as_it_evaluates() {
        let runtime = "export function injectIntoGlobalHook(g) { g.hooked = true; }\n";
        let out = runtime_code(runtime);
        assert!(out.starts_with(runtime), "{out}");
        let tail = &out[runtime.len()..];
        // The preamble plugin-react documents, verbatim, behind a window guard.
        assert!(tail.contains("injectIntoGlobalHook(window);"), "{tail}");
        assert!(tail.contains("window.$RefreshReg$ = () => {};"), "{tail}");
        assert!(
            tail.contains("window.$RefreshSig$ = () => (type) => type;"),
            "{tail}"
        );
        assert!(
            tail.trim_start()
                .starts_with("if (typeof window !== \"undefined\")"),
            "{tail}"
        );
        // The wrapper reads registrations as `$RefreshReg$(`: the stubs are
        // assignments, so the runtime is never wrapped as a component module.
        assert!(!out.contains("$RefreshReg$("), "{out}");
    }
}
