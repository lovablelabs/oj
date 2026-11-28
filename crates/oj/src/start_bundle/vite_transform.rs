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
use rolldown_utils::js_regex::HybridRegex;
use rolldown_utils::pattern_filter::{filter as pattern_filter, StringOrRegex};

pub(crate) struct ViteTransform {
    root: PathBuf,
    /// A server consumer's environment: no Fast Refresh.
    is_server_consumer: bool,
    /// The environment's `oxc` options as Vite resolved them.
    transform_options: BundlerTransformOptions,
    /// The nearest tsconfig of a TypeScript module (rolldown's discovery).
    resolver: Resolver,
    /// `oxc: false`: the plugin is off.
    disabled: bool,
    /// The plugin's filters (`oxc.include`, `oxc.exclude`): with neither,
    /// the module type decides.
    include: Vec<StringOrRegex>,
    exclude: Vec<StringOrRegex>,
    /// The Fast Refresh filters a plugin's config hook adds (plugin-react
    /// names every `.[tj]sx?` module): a module the include names is
    /// transformed as well, whatever the module type.
    jsx_refresh_include: Vec<StringOrRegex>,
    jsx_refresh_exclude: Vec<StringOrRegex>,
}

/// A lowered module: the printed code and its map (JSON), when asked for.
pub(crate) struct Lowered {
    pub code: String,
    pub map: Option<String>,
}

/// The plugin's jsxRefreshFilter answer.
#[derive(PartialEq, Eq)]
enum RefreshFilter {
    /// No refresh filters at all.
    None,
    True,
    False,
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
            disabled: false,
            include: Vec::new(),
            exclude: Vec::new(),
            jsx_refresh_include: Vec::new(),
            jsx_refresh_exclude: Vec::new(),
        }
    }

    /// The filters off the host's `oxc` fact (`Facts::oxc_filters`): each a
    /// pattern or list of patterns, a regex as `{ $regex, flags }`.
    pub fn with_filters(mut self, facts: Option<&serde_json::Value>) -> Self {
        let facts = match facts {
            Some(facts) if facts.as_bool() == Some(false) => {
                self.disabled = true;
                return self;
            }
            Some(facts) => facts,
            // No fact read: Vite's own defaults, as its plugin passes them
            // to the native transform when the config names none.
            None => &serde_json::Value::Null,
        };
        let patterns = |key: &str| -> Vec<StringOrRegex> {
            let value = facts
                .as_object()
                .and_then(|o| o.get(key))
                .unwrap_or(&serde_json::Value::Null);
            let items: Vec<&serde_json::Value> = match value {
                serde_json::Value::Array(items) => items.iter().collect(),
                serde_json::Value::Null => Vec::new(),
                one => vec![one],
            };
            items
                .into_iter()
                .filter_map(|item| match item {
                    serde_json::Value::String(s) => Some(StringOrRegex::String(s.clone())),
                    serde_json::Value::Object(o) => {
                        let source = o.get("$regex")?.as_str()?;
                        let flags = o.get("flags").and_then(|f| f.as_str()).unwrap_or("");
                        HybridRegex::with_flags(source, flags)
                            .ok()
                            .map(StringOrRegex::Regex)
                    }
                    _ => None,
                })
                .collect()
        };
        // vite:oxc hands the plugin its own defaults where the config has
        // none (`include = /\.(m?ts|[jt]sx)$/`, `exclude = /\.js$/`), so the
        // refresh include is consulted for a `.js` module the exclude
        // turned down.
        let default = |pattern: &str| {
            HybridRegex::new_ecma(pattern)
                .map(|re| vec![StringOrRegex::Regex(re)])
                .unwrap_or_default()
        };
        self.include = patterns("include");
        if self.include.is_empty() {
            self.include = default(r"\.(m?ts|[jt]sx)$");
        }
        self.exclude = patterns("exclude");
        if self.exclude.is_empty() {
            self.exclude = default(r"\.js$");
        }
        self.jsx_refresh_include = patterns("jsxRefreshInclude");
        self.jsx_refresh_exclude = patterns("jsxRefreshExclude");
        self
    }

    fn cwd(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }

    /// The plugin's jsxRefreshFilter.
    fn jsx_refresh_filter(&self, id: &str, cwd: &str) -> RefreshFilter {
        if self.jsx_refresh_include.is_empty() && self.jsx_refresh_exclude.is_empty() {
            return RefreshFilter::None;
        }
        let exclude =
            (!self.jsx_refresh_exclude.is_empty()).then_some(self.jsx_refresh_exclude.as_slice());
        let include =
            (!self.jsx_refresh_include.is_empty()).then_some(self.jsx_refresh_include.as_slice());
        if pattern_filter(exclude, include, id, cwd).inner() {
            RefreshFilter::True
        } else {
            RefreshFilter::False
        }
    }

    /// Whether the plugin takes `id` (its `filter`): a virtual id (a NUL
    /// byte) never; with no `include` or `exclude`, a jsx, tsx or ts module
    /// by extension; else the filters over the id and the id without its
    /// query; else a module the refresh include names.
    pub fn handles(&self, id: &str) -> bool {
        if self.disabled || id.contains('\0') {
            return false;
        }
        let cwd = self.cwd();
        if self.include.is_empty() && self.exclude.is_empty() {
            let ext = Path::new(id.split(['?', '#']).next().unwrap_or(id))
                .extension()
                .map(|e| e.to_string_lossy());
            let Some(ext) = ext else {
                return false;
            };
            return matches!(
                ModuleType::from_str_with_fallback(ext.as_ref()),
                ModuleType::Jsx | ModuleType::Tsx | ModuleType::Ts
            );
        }
        let exclude = (!self.exclude.is_empty()).then_some(self.exclude.as_slice());
        let include = (!self.include.is_empty()).then_some(self.include.as_slice());
        if pattern_filter(exclude, include, id, &cwd).inner() {
            return true;
        }
        let cleaned = id.split(['?', '#']).next().unwrap_or(id);
        if cleaned != id && pattern_filter(exclude, include, cleaned, &cwd).inner() {
            return true;
        }
        self.jsx_refresh_filter(id, &cwd) == RefreshFilter::True
    }

    /// The module lowered and printed as the plugin prints it.
    pub fn lower(&self, id: &str, code: &str, sourcemap: bool) -> anyhow::Result<Lowered> {
        let cwd = self.cwd();
        let clean = id.split(['?', '#']).next().unwrap_or(id);
        // vite:oxc reads the lang off the id as written when its tail is a
        // script extension (`App.vue?vue&type=script&lang.ts`), else off
        // the id without its query.
        let raw_ext = Path::new(id).extension().map(|e| e.to_string_lossy());
        let ext = match raw_ext.as_deref() {
            Some("js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts") => raw_ext,
            _ => Path::new(clean).extension().map(|e| e.to_string_lossy()),
        };
        // A module only the refresh include names, with an extension of its
        // own (an .mdx), is read as JavaScript.
        let refresh_lang = self.jsx_refresh_filter(id, &cwd) == RefreshFilter::True
            && !matches!(ext.as_deref(), Some("js" | "jsx" | "mjs" | "ts" | "tsx"));
        let source_type = if refresh_lang {
            SourceType::mjs()
        } else {
            match ext.as_deref() {
                Some("js" | "cjs" | "mjs") => SourceType::mjs(),
                Some("jsx") => SourceType::jsx(),
                Some("ts" | "cts" | "mts") => SourceType::ts(),
                Some("tsx") => SourceType::tsx(),
                _ => anyhow::bail!("Failed to detect the lang of {id}."),
            }
        };
        let mut options = self.transform_options.clone();
        // Fast Refresh is off on the server, off a module the refresh
        // filters leave out, and off a module that is neither JSX by
        // extension nor imports the JSX runtime.
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
                    || self.jsx_refresh_filter(id, &cwd) == RefreshFilter::False
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

    /// A parenthesized arrow in a conditional branch keeps its parentheses
    /// through the lowering, as through rolldown's transform: the parser
    /// marks a parenthesized function (`pife`) with `preserve_parens: false`
    /// too, and the codegen prints the mark. (Under Vite, StyleX's babel
    /// print drops them before this step; see oj_stylex's reprint.)
    #[test]
    fn a_parenthesized_arrow_keeps_its_parentheses_as_rolldowns_transform_does() {
        let src = "export const r = (c: unknown) =>\n  typeof c === \"function\" ? ((...a: unknown[]) => <>{a}</>) : <>{c}</>;\n";
        let out = transform().lower("/app/src/p.tsx", src, false).unwrap();
        assert!(out.code.contains("((...a) =>"), "{}", out.code);
    }

    /// With plugin-react's refresh include (`/\.[tj]sx?$/`), a `.js` module
    /// is transformed too, as rolldown's plugin takes a module the refresh
    /// include names; `oxc: false` turns the plugin off.
    #[test]
    fn the_refresh_include_takes_a_js_module() {
        let facts = serde_json::json!({
            "include": null,
            "exclude": null,
            "jsxRefreshInclude": [{ "$regex": "\\.[tj]sx?$", "flags": "" }, { "$regex": "\\.[tj]sx?\\?", "flags": "" }],
            "jsxRefreshExclude": null,
        });
        let t = transform().with_filters(Some(&facts));
        assert!(t.handles("/app/node_modules/dep/index.js") && t.handles("/app/src/a.js?v=1"));
        assert!(!t.handles("/app/src/a.css") && !t.handles("/app/src/a.json"));
        let out = t
            .lower(
                "/app/src/a.js",
                "// gone\n/** kept */\nexport const a = 1;\n",
                false,
            )
            .unwrap();
        assert_eq!(out.code, "/** kept */\nexport const a = 1;\n");
        assert!(!transform()
            .with_filters(Some(&serde_json::Value::Bool(false)))
            .handles("/app/src/f.tsx"));
    }
}
