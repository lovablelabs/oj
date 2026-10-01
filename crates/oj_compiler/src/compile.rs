use crate::env::{defines_for, ImportMetaEnv};
use crate::hot::lex_hot_accept;
use crate::rewrite::rewrite_module_specifiers;
use crate::scan::{export_names, scan, F_IMPORT_META_GLOB, F_IMPORT_PAREN};
use crate::sourcemap::{compose_input_maps_json, map_json_to_data_url};
use crate::{glob, tsconfig};
use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use oxc_codegen::{Codegen, CodegenOptions, CodegenReturn};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{JsxRuntime, ReactRefreshOptions, TransformOptions, Transformer};
use oxc_transformer_plugins::ReplaceGlobalDefines;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub type ImportRewriter<'r> = dyn FnMut(&str) -> Option<String> + 'r;

pub const COMPILE_STACK_SIZE: usize = 16 * 1024 * 1024;

pub(crate) fn detect_refresh_registrations(program: &Program) -> bool {
    use oxc_ast::ast::{CallExpression, Expression};
    use oxc_ast_visit::{walk, Visit};

    struct Detector {
        found: bool,
    }
    impl<'a> Visit<'a> for Detector {
        fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
            if self.found {
                return;
            }
            if let Expression::Identifier(id) = &call.callee {
                if id.name == "$RefreshReg$" {
                    self.found = true;
                    return;
                }
            }
            walk::walk_call_expression(self, call);
        }
    }
    let mut detector = Detector { found: false };
    detector.visit_program(program);
    detector.found
}

/// JSX compile settings: Vite's `oxc.jsx` (what `@vitejs/plugin-react` sets from
/// its `jsxRuntime`/`jsxImportSource` options) or the older `esbuild.jsx*` form.
/// A file's own `@jsx`, `@jsxRuntime`, `@jsxImportSource` and `@jsxFrag` pragma
/// comments still win, since oxc applies them on top of these.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JsxConfig {
    /// `"classic"` emits `pragma(...)` calls; anything else is the automatic runtime.
    pub runtime: Option<String>,
    /// Package the automatic runtime is imported from (default `react`).
    pub import_source: Option<String>,
    /// Classic-runtime element factory (default `React.createElement`).
    pub pragma: Option<String>,
    /// Classic-runtime fragment (default `React.Fragment`).
    pub pragma_frag: Option<String>,
}

impl JsxConfig {
    pub fn is_classic(&self) -> bool {
        self.runtime.as_deref() == Some("classic")
    }
}

#[derive(Debug, Clone)]
pub struct CompileOptions {
    pub dev: bool,
    pub refresh: bool,
    pub sourcemap: bool,
    pub ssr: bool,
    pub jsx: JsxConfig,
    /// Class-field semantics decided by the caller ([[Set]] when true), so a
    /// cache key computed from the ORIGINAL path and a compile running on a
    /// synthetic one (`x.svg` -> `x.svg.tsx`) can never disagree; `None` asks
    /// the compiler to consult the nearest tsconfig itself.
    pub class_field_set_semantics: Option<bool>,
    /// The dev server's defines; `None` uses the mode-derived fallback.
    pub env: Option<Arc<ImportMetaEnv>>,
}

impl CompileOptions {
    pub fn dev() -> Self {
        Self {
            dev: true,
            refresh: true,
            sourcemap: true,
            ssr: false,
            jsx: JsxConfig::default(),
            class_field_set_semantics: None,
            env: None,
        }
    }

    pub fn prod() -> Self {
        Self {
            dev: false,
            refresh: false,
            sourcemap: true,
            ssr: false,
            jsx: JsxConfig::default(),
            class_field_set_semantics: None,
            env: None,
        }
    }
}

#[derive(Debug)]
pub struct CompileOutput {
    pub code: String,
    /// The sourcemap as RAW JSON; the data URL is built at serve time, as
    /// Vite's genSourceMapUrl does per send. Raw JSON retains 25% fewer bytes
    /// than base64 in every cache that holds the module.
    pub map_json: Option<String>,
    pub imports: Vec<String>,
    pub dynamic_imports: Vec<String>,
    /// Per import (rewritten specifier), the binding names this module uses
    /// from it: named/default imports and re-exports by name, `*` for
    /// namespace and dynamic imports (Vite's `importedBindings`).
    pub import_bindings: Vec<(String, Vec<String>)>,
    pub is_refresh_boundary: bool,
    /// `Some` when the module references `import.meta.hot` (it needs a hot
    /// context injected); what its `accept` calls declared.
    pub hot_accept: Option<HotAccept>,
}

/// The `import.meta.hot.accept(...)` forms a module uses (Vite's
/// `lexAcceptedHmrDeps`): `accept()` / `accept(cb)` make it self-accepting,
/// `accept('./dep', cb)` / `accept(['./a', './b'], cb)` make it the boundary for
/// updates of those dependencies (specifiers already rewritten to served urls).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HotAccept {
    pub self_accepting: bool,
    pub deps: Vec<String>,
    /// `acceptExports([...names], cb)`: the module is a boundary only for
    /// updates its importers reach through these exports (Vite's
    /// `acceptedHmrExports`); an importer using any other export propagates
    /// past it.
    pub accepted_exports: Option<Vec<String>>,
}

impl CompileOutput {
    pub fn code_with_inline_map(&self) -> String {
        match self.map_data_url() {
            Some(url) => format!("{}\n//# sourceMappingURL={}\n", self.code, url),
            None => self.code.clone(),
        }
    }

    /// The inline `data:` form of [`CompileOutput::map_json`].
    pub fn map_data_url(&self) -> Option<String> {
        self.map_json.as_deref().map(map_json_to_data_url)
    }

    pub fn has_refresh_registrations(&self) -> bool {
        self.is_refresh_boundary
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    #[error("unsupported file type: {0}")]
    UnsupportedFileType(PathBuf),
    #[error("parse error in {path}:\n{message}")]
    Parse { path: PathBuf, message: String },
    #[error("transform error in {path}:\n{message}")]
    Transform { path: PathBuf, message: String },
}

pub fn compile(
    path: &Path,
    source_text: &str,
    opts: &CompileOptions,
) -> Result<CompileOutput, CompileError> {
    compile_module(path, source_text, opts, None)
}

pub fn compile_module(
    path: &Path,
    source_text: &str,
    opts: &CompileOptions,
    rewriter: Option<&mut ImportRewriter>,
) -> Result<CompileOutput, CompileError> {
    compile_module_with_maps(path, source_text, opts, rewriter, &[])
}

pub fn compile_module_with_maps(
    path: &Path,
    source_text: &str,
    opts: &CompileOptions,
    mut rewriter: Option<&mut ImportRewriter>,
    input_maps: &[String],
) -> Result<CompileOutput, CompileError> {
    let source_type = SourceType::from_path(path)
        .map_err(|_| CompileError::UnsupportedFileType(path.to_path_buf()))?;

    let allocator = Allocator::default();

    let parsed = Parser::new(&allocator, source_text, source_type).parse();
    if parsed.panicked || !parsed.diagnostics.is_empty() {
        let message = parsed
            .diagnostics
            .into_iter()
            .map(|d| format!("{:?}", d.with_source_code(source_text.to_string())))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(CompileError::Parse {
            path: path.to_path_buf(),
            message,
        });
    }
    let mut program = parsed.program;

    let semantic_ret = SemanticBuilder::new()
        .with_excess_capacity(2.0)
        .with_enum_eval(true)
        .build(&program);
    let scoping = semantic_ret.semantic.into_scoping();

    let mut transform_options = TransformOptions::default();
    // Vite honors the nearest tsconfig's class-field semantics (vite:oxc hands
    // tsconfig discovery to the transform); oxc's documented recipe for
    // `useDefineForClassFields: false` is exactly these two flags.
    if opts
        .class_field_set_semantics
        .unwrap_or_else(|| tsconfig::class_field_set_semantics(path))
    {
        transform_options.assumptions.set_public_class_fields = true;
        transform_options
            .typescript
            .remove_class_fields_without_initializer = true;
    }
    transform_options.jsx.jsx_plugin = true;
    if opts.jsx.is_classic() {
        transform_options.jsx.runtime = JsxRuntime::Classic;
        // oxc rejects pragma/pragmaFrag under the automatic runtime, so they are
        // only set for classic.
        transform_options.jsx.pragma = opts.jsx.pragma.clone();
        transform_options.jsx.pragma_frag = opts.jsx.pragma_frag.clone();
    } else {
        transform_options.jsx.runtime = JsxRuntime::Automatic;
        transform_options.jsx.import_source = opts.jsx.import_source.clone();
    }
    transform_options.jsx.development = opts.dev;
    transform_options.jsx.jsx_self_plugin = opts.dev;
    transform_options.jsx.jsx_source_plugin = opts.dev;
    if opts.dev && opts.refresh {
        transform_options.jsx.refresh = Some(ReactRefreshOptions::default());
    }

    let transform_ret = Transformer::new(&allocator, path, &transform_options)
        .build_with_scoping(scoping, &mut program);
    if !transform_ret.diagnostics.is_empty() {
        let message = transform_ret
            .diagnostics
            .into_iter()
            .map(|d| format!("{:?}", d.with_source_code(source_text.to_string())))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(CompileError::Transform {
            path: path.to_path_buf(),
            message,
        });
    }

    let defines = defines_for(opts.env.as_deref(), opts.dev, opts.ssr);
    if defines.needed_by(source_text) {
        if let Some(config) = defines.config() {
            let scoping = SemanticBuilder::new()
                .build(&program)
                .semantic
                .into_scoping();
            let _ = ReplaceGlobalDefines::new(&allocator, config).build(scoping, &mut program);
        }
    }

    // Track synthesized expansions: they splice generated nodes with no faithful
    // source origin, so their sourcemap must be skipped (see codegen below).
    let mut synthesized = false;
    if F_IMPORT_META_GLOB.find(source_text.as_bytes()).is_some() {
        let dir = path.parent().unwrap_or(path);
        glob::expand(&allocator, dir, &mut program);
        synthesized = true;
    }

    // Expand dynamic-import-vars (import(`./x/${v}.js`)) in dev too — the build
    // path already does; without it these work in `oj build` but throw in dev.
    if scan(&F_IMPORT_PAREN, source_text) {
        let dir = path.parent().unwrap_or(path);
        synthesized |= glob::expand_dynamic_import_vars(&allocator, dir, &mut program, source_text);
    }

    // new URL("./asset", import.meta.url) -> a hoisted ?url asset import.
    if source_text.contains("import.meta.url") {
        let dir = path.parent().unwrap_or(path);
        synthesized |= glob::expand_new_url_asset(&allocator, dir, &mut program, source_text);
    }

    let (imports, dynamic_imports, import_bindings) =
        rewrite_module_specifiers(&allocator, &mut program, &mut rewriter);
    let mut hot_accept = lex_hot_accept(&allocator, &mut program, &mut rewriter);
    // Vite's promotion (importAnalysis): a module whose acceptExports list
    // covers every export it actually has is fully self-accepting, so even a
    // namespace or dynamic importer hot-swaps through it.
    if let Some(h) = hot_accept.as_mut() {
        if let Some(accepted) = &h.accepted_exports {
            // Vacuously true with no detectable exports, as in Vite (its
            // es-module-lexer list is empty for `export *` too).
            if export_names(&program).iter().all(|n| accepted.contains(n)) {
                h.self_accepting = true;
            }
        }
    }

    let is_refresh_boundary = opts.refresh && detect_refresh_registrations(&program);

    // Synthesized nodes (glob / dynamic-import-vars) carry generated-string spans
    // with no origin in this module's source; sourcemapping them panics oxc's
    // builder on out-of-range spans, so skip the map for those modules.
    let codegen_options = CodegenOptions {
        source_map_path: (opts.sourcemap && !synthesized).then(|| path.to_path_buf()),
        ..CodegenOptions::default()
    };
    let CodegenReturn { code, map, .. } =
        Codegen::new().with_options(codegen_options).build(&program);

    let map_json = map.map(|oj_map| {
        let mut json = if input_maps.is_empty() {
            oj_map.to_json_string()
        } else {
            compose_input_maps_json(&oj_map, input_maps)
        };
        // to_json_string over-reserves ~4x; this String is RETAINED per module
        // in every cache, so excess capacity is resident memory.
        json.shrink_to_fit();
        json
    });

    Ok(CompileOutput {
        code,
        map_json,
        imports,
        dynamic_imports,
        import_bindings,
        is_refresh_boundary,
        hot_accept,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn defines_apply_after_jsx_transform_without_reference_id_panic() {
        // import.meta.env inside JSX: the JSX/TS transform introduces
        // IdentifierReferences without reference_ids, and ReplaceGlobalDefines
        // reads reference_id() while walking the whole program. Before scoping
        // was rebuilt on the transformed program this panicked; it must compile
        // and still apply the defines.
        let src = r#"
export function App() {
  return <div className={import.meta.env.DEV ? "dev" : "prod"}>{import.meta.env.MODE}</div>;
}
"#;
        let out = compile_module(Path::new("App.tsx"), src, &CompileOptions::prod(), None).unwrap();
        assert!(
            out.code.contains("false"),
            "import.meta.env.DEV -> false: {}",
            out.code
        );
        assert!(
            out.code.contains("production"),
            "MODE -> production: {}",
            out.code
        );
    }

    #[test]
    fn enum_declarations_compile() {
        // oxc's TS enum transform requires with_enum_eval(true); without it the
        // transformer panics on any enum.
        let src = "export enum Dir { Up, Down }\nexport const d = Dir.Up;";
        let out = compile_module(Path::new("e.ts"), src, &CompileOptions::prod(), None).unwrap();
        assert!(!out.code.is_empty(), "{}", out.code);
    }

    const APP_TSX: &str = r#"
interface Props { label: string }

export function Counter({ label }: Props) {
  const [n, setN] = React.useState<number>(0);
  return <button onClick={() => setN(n + 1)}>{label}: {n}</button>;
}

import React from "react";
"#;

    #[test]
    fn strips_types_and_uses_automatic_runtime_in_prod() {
        let out = compile(Path::new("App.tsx"), APP_TSX, &CompileOptions::prod()).unwrap();
        assert!(!out.code.contains("interface"), "types must be stripped");
        assert!(!out.code.contains("<button"), "JSX must be transformed");
        assert!(
            out.code.contains("react/jsx-runtime"),
            "prod uses the automatic runtime:\n{}",
            out.code
        );
        assert!(out.map_json.is_some());
    }

    #[test]
    fn dev_uses_jsx_dev_runtime_and_instruments_fast_refresh() {
        let out = compile(Path::new("App.tsx"), APP_TSX, &CompileOptions::dev()).unwrap();
        assert!(
            out.code.contains("react/jsx-dev-runtime"),
            "dev uses jsxDEV:\n{}",
            out.code
        );
        assert!(
            out.code.contains("$RefreshReg$"),
            "components must be registered for Fast Refresh:\n{}",
            out.code
        );
        assert!(
            out.code.contains("$RefreshSig$"),
            "hook users must be signed for Fast Refresh:\n{}",
            out.code
        );
    }

    #[test]
    fn reports_parse_errors_instead_of_panicking() {
        let err = compile(
            Path::new("Broken.tsx"),
            "const = <div>;",
            &CompileOptions::dev(),
        )
        .unwrap_err();
        assert!(matches!(err, CompileError::Parse { .. }));
    }

    #[test]
    fn dev_compile_expands_dynamic_import_vars() {
        // import(`./x/${v}.js`) must expand in the DEV compile path too — the
        // build path already does, so without this it works in `oj build` and
        // throws at runtime under `oj dev`.
        let dir = std::env::temp_dir().join(format!("oj-dynimport-{}", std::process::id()));
        let loc = dir.join("locales");
        std::fs::create_dir_all(&loc).unwrap();
        std::fs::write(loc.join("en.json"), "{}").unwrap();
        std::fs::write(loc.join("fr.json"), "{}").unwrap();
        let src = "export const load = (l) => import(`./locales/${l}.json`);\n";
        let out = compile(&dir.join("main.js"), src, &CompileOptions::dev()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            out.code.contains("./locales/en.json"),
            "dyn-import-var must expand in dev:\n{}",
            out.code
        );
        assert!(out.code.contains("./locales/fr.json"), "{}", out.code);
        assert!(
            out.code.contains("case "),
            "expanded to a switch over matches:\n{}",
            out.code
        );
    }

    #[test]
    fn dev_compile_rewrites_new_url_import_meta_url() {
        // new URL("./x", import.meta.url) -> a hoisted ?url asset import ref, so
        // the asset flows through oj's pipeline instead of 404-ing.
        let src = "export const w = new URL(\"./worker.js\", import.meta.url);\n";
        let out = compile(std::path::Path::new("m.js"), src, &CompileOptions::dev()).unwrap();
        assert!(
            out.code.contains("__oj_url_0"),
            "rewritten to hoisted asset ref:\n{}",
            out.code
        );
        assert!(
            out.code.contains("worker.js?url"),
            "hoisted ?url import:\n{}",
            out.code
        );
        assert!(
            !out.code.contains("new URL(\"./worker.js\""),
            "original literal replaced:\n{}",
            out.code
        );
    }

    #[test]
    fn fast_refresh_only_in_dev_with_refresh_enabled() {
        let prod = compile(Path::new("C.tsx"), APP_TSX, &CompileOptions::prod()).unwrap();
        assert!(!prod.has_refresh_registrations());
        let dev_no_refresh = compile_module(
            Path::new("C.tsx"),
            APP_TSX,
            &CompileOptions {
                dev: true,
                refresh: false,
                sourcemap: false,
                ssr: false,
                jsx: JsxConfig::default(),
                class_field_set_semantics: None,
                env: None,
            },
            None,
        )
        .unwrap();
        assert!(!dev_no_refresh.has_refresh_registrations());
        assert!(
            dev_no_refresh.code.contains("jsx-dev-runtime"),
            "dev runtime regardless of refresh"
        );
    }

    #[test]
    fn rejects_unsupported_file_types() {
        let err = compile(Path::new("styles.css"), "body{}", &CompileOptions::prod()).unwrap_err();
        assert!(
            matches!(err, CompileError::UnsupportedFileType(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn jsx_import_source_from_config_and_pragma_comment() {
        let src = "export const A = () => <div>hi</div>;\n";
        let mut opts = CompileOptions::prod();
        opts.jsx.import_source = Some("preact".into());
        let out = compile_module(Path::new("A.tsx"), src, &opts, None).unwrap();
        assert!(
            out.imports.iter().any(|i| i == "preact/jsx-runtime"),
            "{:?}",
            out.imports
        );
        assert!(!out.code.contains("\"react/jsx-runtime\""), "{}", out.code);

        // Dev uses the dev runtime of the same source.
        let mut dev = CompileOptions::dev();
        dev.jsx.import_source = Some("@emotion/react".into());
        let out = compile_module(Path::new("A.tsx"), src, &dev, None).unwrap();
        assert!(
            out.imports
                .iter()
                .any(|i| i == "@emotion/react/jsx-dev-runtime"),
            "{:?}",
            out.imports
        );

        // A file pragma wins over the config (oxc reads leading comments).
        let pragma = format!("/** @jsxImportSource solid-js */\n{src}");
        let out = compile_module(Path::new("A.tsx"), &pragma, &opts, None).unwrap();
        assert!(
            out.imports.iter().any(|i| i == "solid-js/jsx-runtime"),
            "{:?}",
            out.imports
        );
    }

    #[test]
    fn classic_runtime_uses_configured_pragma() {
        let src = "import { h, Fragment } from 'preact';\nexport const A = () => <><b>x</b></>;\n";
        let mut opts = CompileOptions::prod();
        opts.jsx = JsxConfig {
            runtime: Some("classic".into()),
            import_source: None,
            pragma: Some("h".into()),
            pragma_frag: Some("Fragment".into()),
        };
        let out = compile_module(Path::new("A.tsx"), src, &opts, None).unwrap();
        assert!(out.code.contains("h(Fragment"), "{}", out.code);
        assert!(out.code.contains("h(\"b\""), "{}", out.code);
        assert!(
            !out.imports.iter().any(|i| i.contains("jsx-runtime")),
            "{:?}",
            out.imports
        );
    }

    #[test]
    fn sourcemap_toggle_and_inline_map_helper() {
        let no_map = compile_module(
            Path::new("a.ts"),
            "export const x = 1;",
            &CompileOptions {
                dev: false,
                refresh: false,
                sourcemap: false,
                ssr: false,
                jsx: JsxConfig::default(),
                class_field_set_semantics: None,
                env: None,
            },
            None,
        )
        .unwrap();
        assert!(no_map.map_json.is_none());
        assert_eq!(no_map.code_with_inline_map(), no_map.code);

        let with_map = compile(
            Path::new("a.ts"),
            "export const x = 1;",
            &CompileOptions::prod(),
        )
        .unwrap();
        assert!(with_map.map_json.is_some());
        assert!(with_map
            .code_with_inline_map()
            .contains("sourceMappingURL="));
    }
}
