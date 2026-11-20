//! A dev module the StyleX plugin compiles ahead of oj's own transforms, with
//! no other plugin's transform between the two.

use std::borrow::Cow;
use std::path::Path;

use oj_stylex::plugin::{MappedOutput, StylexPlugin};
use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::compile::{compile_text, emit, CompileError, CompileOptions, CompileOutput};
use crate::interop::{
    imports_may_need_interop, may_rewrite, rewrite_cjs_interop_logged, ImporterFormat,
    InteropTarget, LaterTransforms,
};
use crate::sourcemap::InputMap;
use crate::ImportRewriter;

/// The inputs of the CJS interop rewrite (see [`rewrite_cjs_interop_logged`]).
pub struct InteropPass<'a> {
    pub format: ImporterFormat,
    pub later: &'a LaterTransforms,
    pub may_need_interop: &'a dyn Fn(&str) -> bool,
    pub interop: &'a dyn Fn(&str) -> Option<InteropTarget>,
    pub warn: &'a mut dyn FnMut(String),
}

#[derive(Debug, thiserror::Error)]
pub enum StylexModuleError {
    /// The plugin's transform failed: its own message.
    #[error("{0}")]
    Stylex(String),
    #[error(transparent)]
    Compile(#[from] CompileError),
}

/// Compiles a module the StyleX plugin handles as three steps in turn do:
/// the plugin's transform of `source_text`, the CJS interop rewrite of what
/// it wrote, and the compile of that with the plugin's map as its input map
/// (`opts.stylex` is not read).
///
/// A TypeScript module none of whose imports may need interop takes them in
/// one parse instead: the plugin compiles the program in place and the
/// compile carries on from it. The module it emits is the steps' own, with
/// one difference: its positions (the source map, the `lineNumber` and
/// `columnNumber` of a dev JSX element) are read off `source_text` itself,
/// where the steps read them off the plugin's output and fold the map back.
/// Whatever the one parse cannot answer for (another language, a diagnostic,
/// an import that may need interop, any error) goes through the steps, so an
/// error is always theirs.
pub fn compile_stylex_module(
    path: &Path,
    source_text: &str,
    opts: &CompileOptions,
    stylex: &StylexPlugin,
    interop: InteropPass<'_>,
    rewriter: &mut ImportRewriter,
) -> Result<CompileOutput, StylexModuleError> {
    // The parse and its arena end with this block: the steps past the
    // plugin's transform parse what it wrote into arenas of their own, and
    // none of them is held beside this one.
    let compiled = {
        let allocator = Allocator::default();
        let module = Module {
            allocator: &allocator,
            path,
            source_text,
            opts,
            stylex,
        };
        match module.parse() {
            Some((program, source_type)) if !module.may_rewrite(&program, &interop) => {
                if let Some(output) = module.in_one_pass(program, source_type, &interop, rewriter) {
                    return Ok(output);
                }
                // The plugin compiled the parse in place: the steps start
                // over from the source.
                None
            }
            Some((program, _)) => Some(module.stylex_step(&program)),
            None => None,
        }
    };
    let compiled = compiled
        .unwrap_or_else(|| stylex.transform_source_mapped(path, source_text, opts.sourcemap))
        .map_err(StylexModuleError::Stylex)?;
    finish_steps(path, source_text, opts, compiled, interop, rewriter)
}

struct Module<'a> {
    allocator: &'a Allocator,
    path: &'a Path,
    source_text: &'a str,
    opts: &'a CompileOptions,
    stylex: &'a StylexPlugin,
}

impl<'a> Module<'a> {
    /// One parse both the plugin and the compile can read, with the language
    /// the compile takes the path for; `None` when no parse serves both.
    ///
    /// The plugin reads every module as TSX. That is the compile's own parse
    /// of a `.tsx` file, and of a `.ts` file holding no JSX: the two grammars
    /// part only at a `<` that opens an expression, where TSX takes an
    /// element, and a source naming no closing tag (`</`, `/>`) that parses
    /// as TSX holds none. The compile takes no module with a diagnostic, and
    /// reads a file with no module syntax as a script, which the plugin's
    /// output is no longer once it imports its runtime: only what parses as
    /// a module is one either way.
    fn parse(&self) -> Option<(Program<'a>, SourceType)> {
        let source_type = SourceType::from_path(self.path)
            .ok()
            .filter(|t| t.is_typescript() && !t.is_typescript_definition() && t.is_unambiguous())?;
        let markup_free = || !self.source_text.contains("</") && !self.source_text.contains("/>");
        if !source_type.is_jsx() && !markup_free() {
            return None;
        }
        let parsed =
            Parser::new(self.allocator, self.source_text, source_type.with_jsx(true)).parse();
        let shared = !parsed.fatal_error
            && parsed.diagnostics.is_empty()
            && parsed.program.source_type.is_module();
        shared.then_some((parsed.program, source_type))
    }

    /// Whether the interop rewrite could take the module `program` is the
    /// parse of (see [`may_rewrite`]).
    fn may_rewrite(&self, program: &Program<'_>, interop: &InteropPass<'_>) -> bool {
        may_rewrite(
            program,
            self.source_text,
            interop.later,
            interop.may_need_interop,
        )
    }

    fn in_one_pass(
        &self,
        mut program: Program<'a>,
        source_type: SourceType,
        interop: &InteropPass<'_>,
        rewriter: &mut ImportRewriter,
    ) -> Option<CompileOutput> {
        let rules = self
            .stylex
            .compile_program(self.allocator, &mut program, self.path, self.source_text)
            .ok()?;
        // The plugin adds static imports only (its runtime's), so the dynamic
        // ones stand as they were checked.
        if rules.is_some()
            && imports_may_need_interop(
                &program,
                self.source_text,
                interop.later,
                interop.may_need_interop,
            )
        {
            return None;
        }
        program.source_type = program.source_type.with_standard(!source_type.is_jsx());
        let mut output = emit(
            self.allocator,
            self.path,
            self.source_text,
            self.opts,
            &mut program,
            Some(rewriter),
            &[],
        )
        .ok()?;
        output.stylex_rules = rules.unwrap_or_default();
        Some(output)
    }

    /// The first of the steps over the module's parse, where
    /// [`Self::parse`] made one the plugin has not touched: its transform,
    /// spared a parse of its own.
    fn stylex_step(&self, program: &Program<'_>) -> Result<Option<MappedOutput>, String> {
        self.stylex
            .transform_parsed(program, self.path, self.source_text, self.opts.sourcemap)
    }
}

/// The steps past the plugin's transform, which made `compiled` of
/// `source_text` (`None`: nothing to compile): the CJS interop rewrite and
/// the compile.
fn finish_steps(
    path: &Path,
    source_text: &str,
    opts: &CompileOptions,
    compiled: Option<MappedOutput>,
    interop: InteropPass<'_>,
    rewriter: &mut ImportRewriter,
) -> Result<CompileOutput, StylexModuleError> {
    let (code, mappings, rules) = match compiled {
        Some(out) => (Cow::Owned(out.code), out.mappings, out.rules),
        None => (Cow::Borrowed(source_text), None, Vec::new()),
    };
    let interopped = rewrite_cjs_interop_logged(
        &code,
        path,
        interop.format,
        interop.later,
        interop.may_need_interop,
        interop.interop,
        interop.warn,
    );
    let input_maps: Vec<InputMap> = mappings
        .into_iter()
        .map(|mappings| InputMap::Parsed(mappings.into_sourcemap(Some(source_text))))
        .collect();
    let mut output = compile_text(
        path,
        interopped.as_deref().unwrap_or(&code),
        opts,
        Some(rewriter),
        &input_maps,
        None,
    )?;
    output.stylex_rules = rules;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_module_with_maps;
    use crate::interop::InteropKind;

    const CARD: &str = r#"import * as stylex from '@stylexjs/stylex';
import type { ReactNode } from 'react';

// Styles for the card.
const styles = stylex.create({
  // the root
  root: {
    color: 'red', /* inline */
    backgroundColor: { default: 'white', ':hover': 'blue' },
  },
  title: {
    fontWeight: 700,
  },
});

const fade = stylex.keyframes({
  from: { opacity: 0 },
  to: { opacity: 1 },
});

/** A card. */
export function Card({ title, children }: { title: string; children?: ReactNode }) {
  // spread
  return (
    <section {...stylex.props(styles.root)}>
      <h2 {...stylex.props(styles.title)} style={{ animationName: fade }}>{title}</h2>
      {children}
    </section>
  );
}
"#;

    const COUNTER: &str = r#"import * as stylex from '@stylexjs/stylex';
import { useState } from 'react';

const styles = stylex.create({
  count: {
    color: 'red',
    fontWeight: 700,
  },
});

export function Counter() {
  const [n, setN] = useState(0);
  return <button {...stylex.props(styles.count)} onClick={() => setN(n + 1)}>{n}</button>;
}
"#;

    fn plugin() -> StylexPlugin {
        StylexPlugin::new(&serde_json::json!({}), Path::new("/app"), true, true).unwrap()
    }

    fn opts_under(later: &LaterTransforms) -> CompileOptions {
        let mut opts = CompileOptions::dev();
        opts.class_field_set_semantics = Some(false);
        opts.verbatim_module_syntax = Some(later.keep_value_imports);
        opts
    }

    fn needs_interop(spec: &str) -> bool {
        spec == "react"
    }

    fn interop(spec: &str) -> Option<InteropTarget> {
        needs_interop(spec).then(|| InteropTarget::new("/deps/react.js", InteropKind::PreBundle))
    }

    fn served(spec: &str) -> Option<String> {
        Some(format!("/served/{spec}"))
    }

    fn compile(path: &str, source: &str) -> Result<CompileOutput, String> {
        compile_under(path, source, &LaterTransforms::default())
    }

    fn compile_under(
        path: &str,
        source: &str,
        later: &LaterTransforms,
    ) -> Result<CompileOutput, String> {
        compile_stylex_module(
            Path::new(path),
            source,
            &opts_under(later),
            &plugin(),
            InteropPass {
                format: ImporterFormat::ESM,
                later,
                may_need_interop: &needs_interop,
                interop: &interop,
                warn: &mut |_| {},
            },
            &mut served,
        )
        .map_err(|err| match err {
            StylexModuleError::Stylex(message) => format!("stylex: {message}"),
            StylexModuleError::Compile(err) => format!("compile: {err}"),
        })
    }

    /// The three steps over strings, as the dev server ran them before the
    /// compile took the plugin's transform: the plugin's own serialized map
    /// is the compile's input map.
    fn steps(path: &str, source: &str) -> Result<CompileOutput, String> {
        steps_under(path, source, &LaterTransforms::default())
    }

    fn steps_under(
        path: &str,
        source: &str,
        later: &LaterTransforms,
    ) -> Result<CompileOutput, String> {
        let path = Path::new(path);
        let pre = plugin()
            .transform_source(path, source)
            .map_err(|message| format!("stylex: {message}"))?;
        let (code, maps, rules) = match pre {
            Some(out) => (out.code, out.map.into_iter().collect(), out.rules),
            None => (source.to_string(), Vec::new(), Vec::new()),
        };
        let interopped = rewrite_cjs_interop_logged(
            &code,
            path,
            ImporterFormat::ESM,
            later,
            &needs_interop,
            &interop,
            &mut |_| {},
        );
        let mut output = compile_module_with_maps(
            path,
            interopped.as_deref().unwrap_or(&code),
            &opts_under(later),
            Some(&mut served),
            &maps,
        )
        .map_err(|err| format!("compile: {err}"))?;
        output.stylex_rules = rules;
        Ok(output)
    }

    /// `code` without the positions a dev JSX element carries.
    fn without_jsx_positions(code: &str) -> String {
        let mut out = String::with_capacity(code.len());
        let mut rest = code;
        while let Some(at) = ["lineNumber: ", "columnNumber: "]
            .iter()
            .filter_map(|key| rest.find(key).map(|at| at + key.len()))
            .min()
        {
            out.push_str(&rest[..at]);
            rest = rest[at..].trim_start_matches(|c: char| c.is_ascii_digit());
        }
        out + rest
    }

    fn rules(output: &CompileOutput) -> Vec<String> {
        output
            .stylex_rules
            .iter()
            .map(|rule| format!("{rule:?}"))
            .collect()
    }

    fn assert_same_module(one: &CompileOutput, steps: &CompileOutput) {
        assert_eq!(
            without_jsx_positions(&one.code),
            without_jsx_positions(&steps.code)
        );
        assert_eq!(rules(one), rules(steps));
        assert_eq!(one.imports, steps.imports);
        assert_eq!(one.dynamic_imports, steps.dynamic_imports);
        assert_eq!(one.import_bindings, steps.import_bindings);
        assert_eq!(one.is_refresh_boundary, steps.is_refresh_boundary);
        assert_eq!(one.hot_accept, steps.hot_accept);
    }

    #[test]
    fn one_parse_emits_the_module_the_steps_emit() {
        let one = compile("/app/src/Card.tsx", CARD).unwrap();
        let steps = steps("/app/src/Card.tsx", CARD).unwrap();
        assert_same_module(&one, &steps);
        assert!(!one.stylex_rules.is_empty());
        assert!(!one.code.contains("stylex.create"), "{}", one.code);

        // Its positions are the source's own: the `<section>` on its line
        // there, which the plugin's output (the create call folded onto one
        // line) puts higher up.
        let line = 1 + CARD.lines().position(|l| l.contains("<section")).unwrap();
        assert!(
            one.code.contains(&format!("lineNumber: {line},")),
            "{}",
            one.code
        );
        assert!(!steps.code.contains(&format!("lineNumber: {line},")));
        let map =
            oxc_sourcemap::SourceMap::from_json_string(one.map_json.as_deref().unwrap()).unwrap();
        assert_eq!(map.get_source(0), Some("/app/src/Card.tsx"));
        assert_eq!(map.get_source_content(0), Some(CARD));
    }

    #[test]
    fn one_parse_serves_the_other_shapes_the_steps_do() {
        for (path, source) in [
            (
                "/app/src/styles.ts",
                "import * as stylex from '@stylexjs/stylex';\n\
                 export const styles = stylex.create({\n  row: { display: 'flex' },\n});\n\
                 export const first = <T,>(items: T[]): T | undefined => items[0];\n\
                 export type Row = keyof typeof styles;\n",
            ),
            (
                "/app/src/Plain.tsx",
                "import * as stylex from '@stylexjs/stylex';\n\
                 export const Plain = () => <div />;\n",
            ),
            (
                "/app/src/Lazy.tsx",
                "import * as stylex from '@stylexjs/stylex';\n\
                 const s = stylex.create({\n  a: { color: 'red' },\n});\n\
                 export const load = () => import('./lazy');\n\
                 export const A = () => <i {...stylex.props(s.a)} />;\n\
                 if (import.meta.hot) import.meta.hot.accept();\n",
            ),
        ] {
            let one = compile(path, source).unwrap();
            let steps = steps(path, source).unwrap();
            assert_same_module(&one, &steps);
        }
    }

    #[test]
    fn a_module_whose_imports_need_interop_goes_through_the_steps() {
        for (path, source) in [
            ("/app/src/Counter.tsx", COUNTER.to_string()),
            (
                "/app/src/Lazy.tsx",
                "import * as stylex from '@stylexjs/stylex';\n\
                 const s = stylex.create({\n  a: { color: 'red' },\n});\n\
                 export const load = () => import ( 'react' );\n\
                 export const A = () => <i {...stylex.props(s.a)} />;\n"
                    .to_string(),
            ),
            (
                "/app/src/again.ts",
                "import * as stylex from '@stylexjs/stylex';\n\
                 export const s = stylex.create({\n  a: { color: 'red' },\n});\n\
                 export { useState } from 'react';\n"
                    .to_string(),
            ),
            (
                "/app/src/Card.jsx",
                CARD.replace(": { title: string; children?: ReactNode }", "")
                    .replace("import type { ReactNode } from 'react';\n", ""),
            ),
        ] {
            let module = compile(path, &source).unwrap();
            let steps = steps(path, &source).unwrap();
            assert_eq!(module.code, steps.code);
            assert_eq!(module.map_json, steps.map_json);
            assert!(module.map_json.is_some());
            assert_eq!(rules(&module), rules(&steps));
            assert_eq!(module.imports, steps.imports);
        }
        let counter = compile("/app/src/Counter.tsx", COUNTER).unwrap();
        assert!(counter.code.contains("__ojcjs0"), "{}", counter.code);
    }

    #[test]
    fn a_type_import_of_a_dep_that_needs_interop_keeps_the_one_parse() {
        let allocator = Allocator::default();
        let later = LaterTransforms::default();
        let parsed = Parser::new(&allocator, CARD, SourceType::tsx()).parse();
        assert!(!may_rewrite(&parsed.program, CARD, &later, &needs_interop));
        let parsed = Parser::new(&allocator, COUNTER, SourceType::tsx()).parse();
        assert!(may_rewrite(
            &parsed.program,
            COUNTER,
            &later,
            &needs_interop
        ));
    }

    /// [`CARD`] with its create calls on one line each and every element
    /// ahead of the styles on its line, so the plugin's output has each
    /// where the source does and the two ways agree on the positions too.
    const FLAT_CARD: &str = r#"import * as stylex from '@stylexjs/stylex';
import type { ReactNode } from 'react';

const styles = stylex.create({ root: { color: 'red' }, title: { fontWeight: 700 } });

export function Card({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <section {...stylex.props(styles.root)}>
      <h2 {...stylex.props(styles.title)}>{title}</h2>
      {children}
    </section>
  );
}
"#;

    const TYPE_IMPORT: &str = "import type { ReactNode } from 'react';";

    /// The imports of a dep that needs interop the rewrite never asks about:
    /// the TypeScript transform leaves no binding of them.
    const REMOVED_IMPORTS: &[&str] = &[
        TYPE_IMPORT,
        "import type * as React from 'react'; type ReactNode = React.ReactNode;",
        "import { type ReactNode } from 'react';",
        "import { type ReactNode, type FC as Component } from 'react';",
        // Written as values, used as types or not at all.
        "import { ReactNode } from 'react';",
        "import { ReactNode, useState } from 'react';",
        "import * as R from 'react'; type ReactNode = R.ReactNode;",
        "import Default, { ReactNode } from 'react'; type D = typeof Default;",
    ];

    #[test]
    fn an_import_the_transform_removes_keeps_the_one_parse() {
        let line = 1 + CARD.lines().position(|l| l.contains("<section")).unwrap();
        for import in REMOVED_IMPORTS {
            // The one parse took the module: its positions are the source's.
            let card = CARD.replace(TYPE_IMPORT, import);
            let one = compile("/app/src/Card.tsx", &card).unwrap();
            let steps = steps("/app/src/Card.tsx", &card).unwrap();
            assert!(
                one.code.contains(&format!("lineNumber: {line},")),
                "{import}\n{}",
                one.code
            );
            assert!(!steps.code.contains(&format!("lineNumber: {line},")));
            assert_same_module(&one, &steps);
            assert!(!one.code.contains("react.js"), "{import}\n{}", one.code);

            // And where the positions agree, it is the steps' byte for byte.
            let flat = FLAT_CARD.replace(TYPE_IMPORT, import);
            let one = compile("/app/src/Card.tsx", &flat).unwrap();
            let steps = self::steps("/app/src/Card.tsx", &flat).unwrap();
            assert_eq!(one.code, steps.code, "{import}");
            assert_same_module(&one, &steps);
            assert!(!one.stylex_rules.is_empty());
        }
    }

    #[test]
    fn an_import_the_transform_keeps_goes_through_the_steps() {
        let verbatim = LaterTransforms {
            keep_value_imports: true,
            ..Default::default()
        };
        let default = LaterTransforms::default();
        for (import, later) in [
            // `verbatimModuleSyntax` keeps what is written as a value.
            ("import { type ReactNode } from 'react';", &verbatim),
            ("import { ReactNode } from 'react';", &verbatim),
            // A value refers to one of the bindings.
            (
                "import { ReactNode, useState } from 'react'; export const use = useState;",
                &default,
            ),
            // The JSX transform's `React`.
            (
                "import React from 'react'; type ReactNode = React.ReactNode;",
                &default,
            ),
            // No binding to remove.
            ("import 'react'; type ReactNode = unknown;", &default),
        ] {
            let allocator = Allocator::default();
            let card = FLAT_CARD.replace(TYPE_IMPORT, import);
            let parsed = Parser::new(&allocator, &card, SourceType::tsx()).parse();
            assert!(
                may_rewrite(&parsed.program, &card, later, &needs_interop),
                "{import}"
            );
            let module = compile_under("/app/src/Card.tsx", &card, later).unwrap();
            let steps = steps_under("/app/src/Card.tsx", &card, later).unwrap();
            assert_eq!(module.code, steps.code, "{import}");
            assert_eq!(module.map_json, steps.map_json, "{import}");
            assert_eq!(module.imports, steps.imports);
            assert!(
                module.code.contains("react.js"),
                "{import}\n{}",
                module.code
            );
        }
    }

    /// Uses of an import the search for a plain one does not see (see
    /// `plainly_uses`): the symbols find them, and the module's interop is
    /// the steps'.
    #[test]
    fn a_use_only_the_symbols_find_goes_through_the_steps() {
        const STYLES: &str = "import * as stylex from '@stylexjs/stylex';\n\
             const s = stylex.create({\n  a: { color: 'red' },\n});\n\
             export const A = () => <i {...stylex.props(s.a)} />;\n";
        let later = LaterTransforms::default();
        for (above, below) in [
            // Spread.
            (
                "import defaults from 'react';",
                "export const all = { ...defaults };",
            ),
            (
                "import { items } from 'react';",
                "export const all = [...items];",
            ),
            (
                "import { parts } from 'react';",
                "export const most = Math.max(...parts);",
            ),
            (
                "import * as props from 'react';",
                "export const B = () => <b {...props} />;",
            ),
            // Above the import, called and read.
            (
                "export function made() { return create(); }",
                "import create from 'react';",
            ),
            (
                "export function maker() { return create; }",
                "import create from 'react';",
            ),
            (
                "export const all = () => [...items];",
                "import { items } from 'react';",
            ),
            // Written with an escape.
            (
                "import { useState } from 'react';",
                "export const useCount = () => use\\u0053tate(0);",
            ),
            (
                "import { useState } from 'react';",
                "export const use = use\\u{53}tate;",
            ),
            (
                "import { use\\u0053tate } from 'react';",
                "export const use = { ...useState };",
            ),
        ] {
            let source = format!("{above}\n{STYLES}{below}\n");
            let allocator = Allocator::default();
            let parsed = Parser::new(&allocator, &source, SourceType::tsx()).parse();
            assert!(parsed.diagnostics.is_empty(), "{source}");
            assert!(
                may_rewrite(&parsed.program, &source, &later, &needs_interop),
                "{source}"
            );
            let module = compile("/app/src/A.tsx", &source).unwrap();
            let steps = steps("/app/src/A.tsx", &source).unwrap();
            assert_eq!(module.code, steps.code, "{source}");
            assert_eq!(module.map_json, steps.map_json, "{source}");
            assert!(module.map_json.is_some());
            assert_eq!(rules(&module), rules(&steps));
            assert_eq!(module.imports, steps.imports);
            assert!(!module.stylex_rules.is_empty());
            // The binding is read off the dep's `module.exports`.
            assert!(
                module.code.contains("__ojcjs0") && module.code.contains("react.js"),
                "{source}\n{}",
                module.code
            );
            assert!(!module.code.contains("from 'react'"), "{}", module.code);
        }
    }

    #[test]
    fn errors_are_the_steps_own() {
        for (path, source) in [
            // The plugin's: a value it cannot evaluate.
            (
                "/app/src/Bad.tsx",
                "import * as stylex from '@stylexjs/stylex';\n\
                 const s = stylex.create({ a: { color: window.color } });\n\
                 export const A = () => <i {...stylex.props(s.a)} />;\n",
            ),
            // The plugin's: no TSX (a type assertion).
            (
                "/app/src/cast.ts",
                "import * as stylex from '@stylexjs/stylex';\n\
                 export const s = stylex.create({ a: { color: 'red' } });\n\
                 export const n = <number>(s as unknown);\n",
            ),
            // The compile's, past the plugin's transform.
            (
                "/app/src/Twice.tsx",
                "import * as stylex from '@stylexjs/stylex';\n\
                 const s = stylex.create({\n  a: { color: 'red' },\n});\n\
                 const twice = 1;\n\
                 const twice = 2;\n\
                 export const A = () => <i {...stylex.props(s.a)} />;\n",
            ),
        ] {
            let module = compile(path, source).unwrap_err();
            let steps = steps(path, source).unwrap_err();
            assert_eq!(module, steps);
        }
        let twice = compile(
            "/app/src/Twice.tsx",
            "import * as stylex from '@stylexjs/stylex';\n\
             const twice = 1;\n\
             const twice = 2;\n",
        )
        .unwrap_err();
        assert!(twice.starts_with("compile: parse error in "), "{twice}");
        let bad = compile(
            "/app/src/Bad.tsx",
            "import * as stylex from '@stylexjs/stylex';\n\
             const s = stylex.create({ a: { color: window.color } });\n",
        )
        .unwrap_err();
        assert!(bad.starts_with("stylex: @stylexjs/unplugin: "), "{bad}");
    }

    #[test]
    fn the_plugin_and_the_compile_read_typescript_alike() {
        // What `Module::parse` rests on: the plugin's TSX is the compile's
        // own language for a `.tsx` path, and for a `.ts` one but for JSX.
        let same_grammar = |a: SourceType, b: SourceType| {
            a.is_typescript() == b.is_typescript()
                && a.is_typescript_definition() == b.is_typescript_definition()
                && a.is_jsx() == b.is_jsx()
                && a.module_kind() == b.module_kind()
        };
        let tsx = SourceType::from_path("a.tsx").unwrap();
        assert!(same_grammar(tsx, SourceType::tsx()));
        let ts = SourceType::from_path("a.ts").unwrap();
        assert!(!ts.is_jsx());
        assert!(same_grammar(ts.with_jsx(true), SourceType::tsx()));
        for other in ["a.js", "a.jsx", "a.mts", "a.cts", "a.d.ts"] {
            let other = SourceType::from_path(other).unwrap();
            assert!(!same_grammar(other.with_jsx(true), SourceType::tsx()));
        }
    }
}
