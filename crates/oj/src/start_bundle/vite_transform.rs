// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//! Vite's `vite:oxc` as a build runs it: rolldown's `builtin:vite-transform`
//! plugin (rolldown_plugin_vite_transform, which `viteTransformPlugin` wraps
//! for a bundled environment), at its slot in the plugin chain, after the
//! "pre" plugins and before the normal and post ones. It lowers TypeScript
//! and JSX and prints the module anew, without its ordinary comments; that
//! text is what rolldown parses, measures for `codeSplitting` groups and
//! hands the later plugins, and the module goes on as JavaScript. oj lowered
//! inside rolldown instead, which keeps the loaded text: a module stayed its
//! TypeScript source's size, so a group split into chunks at other modules
//! than under `vite build`, and every hash downstream moved.

use std::path::{Path, PathBuf};

use oxc_codegen::{Codegen, CodegenOptions, CommentOptions};
use oxc_parser::{ParseOptions, Parser};
use oxc_resolver::{ResolveOptions, Resolver, TsconfigDiscovery};
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::Transformer;
use rolldown_common::{BundlerTransformOptions, Either, ModuleType};

pub(crate) struct ViteTransform {
    root: PathBuf,
    /// A server consumer's environment: no Fast Refresh.
    is_server_consumer: bool,
    /// The environment's `oxc` options as Vite resolved them.
    transform_options: BundlerTransformOptions,
    /// The nearest tsconfig of a TypeScript module (rolldown's discovery).
    resolver: Resolver,
}

/// A lowered module: the printed code and its map (JSON), when asked for.
pub(crate) struct Lowered {
    pub code: String,
    pub map: Option<String>,
}

impl ViteTransform {
    pub fn new(
        root: PathBuf,
        is_server_consumer: bool,
        transform_options: BundlerTransformOptions,
    ) -> Self {
        Self {
            root,
            is_server_consumer,
            transform_options,
            resolver: Resolver::new(ResolveOptions {
                tsconfig: Some(TsconfigDiscovery::Auto),
                ..ResolveOptions::default()
            }),
        }
    }

    /// Whether the plugin takes `id`: Vite's default filter (no `oxc.include`
    /// or `exclude`) is the module type off the extension, jsx, tsx or ts;
    /// a virtual id (a NUL byte) never.
    pub fn handles(&self, id: &str) -> bool {
        if id.contains('\0') {
            return false;
        }
        let ext = Path::new(id.split(['?', '#']).next().unwrap_or(id))
            .extension()
            .map(|e| e.to_string_lossy());
        let Some(ext) = ext else {
            return false;
        };
        matches!(
            ModuleType::from_str_with_fallback(ext.as_ref()),
            ModuleType::Jsx | ModuleType::Tsx | ModuleType::Ts
        )
    }

    /// The module lowered and printed as the plugin prints it.
    pub fn lower(&self, id: &str, code: &str, sourcemap: bool) -> anyhow::Result<Lowered> {
        let clean = id.split(['?', '#']).next().unwrap_or(id);
        let ext = Path::new(clean).extension().map(|e| e.to_string_lossy());
        let source_type = match ext.as_deref() {
            Some("js" | "cjs" | "mjs") => SourceType::mjs(),
            Some("jsx") => SourceType::jsx(),
            Some("ts" | "cts" | "mts") => SourceType::ts(),
            Some("tsx") => SourceType::tsx(),
            _ => anyhow::bail!("Failed to detect the lang of {id}."),
        };
        let mut options = self.transform_options.clone();
        // Fast Refresh is off on the server, and in a module that is neither
        // JSX by extension nor imports the JSX runtime.
        if let Some(Either::Right(jsx)) = &mut options.jsx {
            if jsx.refresh.is_some() {
                let import_source = self
                    .transform_options
                    .jsx
                    .as_ref()
                    .and_then(|j| match j {
                        Either::Right(jsx) => jsx.import_source.as_deref(),
                        Either::Left(_) => None,
                    })
                    .unwrap_or("react");
                let imports_runtime = code.contains(&format!("{import_source}/jsx-runtime"))
                    || code.contains(&format!("{import_source}/jsx-dev-runtime"));
                let disabled = self.is_server_consumer
                    || !(ext.as_deref().is_some_and(|e| e.ends_with('x')) || imports_runtime);
                if disabled {
                    jsx.refresh = None;
                }
            }
        }
        if source_type.is_typescript() {
            if let Some(tsconfig) = self
                .resolver
                .find_tsconfig(self.root.join(clean))
                .map_err(|e| anyhow::anyhow!("{id}: {e}"))?
            {
                let (merged, _warnings) =
                    rolldown_common::merge_tsconfig(options, &tsconfig, false);
                options = merged;
            }
        }
        let options: oxc_transformer::TransformOptions = options
            .try_into()
            .map_err(|e: String| anyhow::anyhow!("{id}: {e}"))?;

        let allocator = oxc_allocator::Allocator::default();
        let parsed = Parser::new(&allocator, code, source_type)
            .with_options(ParseOptions {
                preserve_parens: false,
                ..ParseOptions::default()
            })
            .parse();
        if parsed.fatal_error || !parsed.diagnostics.is_empty() {
            let first = parsed
                .diagnostics
                .first()
                .map(|d| d.message.to_string())
                .unwrap_or_else(|| "parse error".to_string());
            anyhow::bail!("{id}: {first}");
        }
        let mut program = parsed.program;
        let scoping = SemanticBuilder::new()
            .with_enum_eval(true)
            .build(&program)
            .semantic
            .into_scoping();
        let ret = Transformer::new(&allocator, Path::new(id), &options)
            .build_with_scoping(scoping, &mut program);
        if ret.diagnostics.has_errors() {
            let first = ret
                .diagnostics
                .iter()
                .find(|d| d.severity == oxc_diagnostics::Severity::Error)
                .map(|d| d.message.to_string())
                .unwrap_or_else(|| "transform error".to_string());
            anyhow::bail!("{id}: {first}");
        }
        let out = Codegen::new()
            .with_options(CodegenOptions {
                comments: CommentOptions {
                    normal: false,
                    ..CommentOptions::default()
                },
                source_map_path: sourcemap.then(|| PathBuf::from(id)),
                ..CodegenOptions::default()
            })
            .build(&program);
        Ok(Lowered {
            code: out.code,
            map: out.map.map(|m| m.to_json_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transform() -> ViteTransform {
        let jsx = rolldown_common::JsxOptions {
            runtime: Some("automatic".to_string()),
            development: Some(false),
            ..Default::default()
        };
        ViteTransform::new(
            PathBuf::from("/app"),
            false,
            BundlerTransformOptions {
                jsx: Some(Either::Right(jsx)),
                ..Default::default()
            },
        )
    }

    /// The plugin prints the lowered module with its ordinary comments
    /// dropped and its JSDoc kept, as rolldown's codegen does for it.
    #[test]
    fn prints_as_rolldowns_vite_transform_does() {
        let src = "// a note\nimport type { T } from \"./t\";\n/** doc */\nexport function f(x: number): T {\n  // inner\n  return <div>{x}</div>;\n}\n";
        let out = transform().lower("/app/src/f.tsx", src, true).unwrap();
        assert!(!out.code.contains("// a note") && !out.code.contains("// inner"));
        assert!(out.code.contains("/** doc */"));
        assert!(
            out.code.contains("react/jsx-runtime") && out.code.contains("jsx("),
            "{}",
            out.code
        );
        assert!(!out.code.contains("import type"));
        assert!(out.map.is_some());
        assert!(transform().handles("/app/src/f.tsx") && transform().handles("/app/x.ts?v=1"));
        assert!(!transform().handles("/app/src/f.js") && !transform().handles("\0virtual"));
    }
}
