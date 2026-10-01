// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ExportAllDeclaration, ExportFromDeclaration, Expression, ImportDeclaration,
    ImportDeclarationSpecifier, ImportExpression, ModuleExportName, Statement,
};
use oxc_ast_visit::{walk, Visit};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};

/// Cheap pre-gate for `rewrite_cjs_interop` on hot paths: the bare specifiers
/// this source may import, scanned without a parse (Vite's economy: its lexer
/// finds the imports, the full parse runs only on the ones being rewritten).
/// The caller probes each candidate against its interop mapping and skips the
/// parse when none maps. Tolerates everything legal between keyword and
/// specifier: Unicode whitespace, comments, and nested parens on `import(`.
/// Over-collection is harmless (the probe returns None).
pub fn bare_import_specifiers(source: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = source.as_bytes();
    for kw in ["from", "import"] {
        let mut at = 0;
        while let Some(pos) = source[at..].find(kw) {
            let i = at + pos;
            at = i + kw.len();
            if !is_keyword_start(bytes, i) {
                continue;
            }
            let s = skip_trivia(source, at, kw == "import");
            let Some(spec) = quoted_at(source, s) else {
                continue;
            };
            if !spec.is_empty()
                && !spec.starts_with('.')
                && !spec.starts_with('/')
                && !spec.contains('\\')
                && !out.iter().any(|o| o == spec)
            {
                out.push(spec.to_string());
            }
        }
    }
    out
}

/// A keyword match at `i`, not the tail of an identifier.
fn is_keyword_start(bytes: &[u8], i: usize) -> bool {
    i == 0
        || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_' || bytes[i - 1] == b'$')
}

/// Skips whitespace and comments (and, when `parens`, any '(' runs) from byte
/// offset `i`; returns the offset of the next significant char.
fn skip_trivia(source: &str, mut i: usize, parens: bool) -> usize {
    loop {
        let rest = &source[i..];
        let Some(c) = rest.chars().next() else {
            return i;
        };
        if c.is_whitespace() {
            i += c.len_utf8();
        } else if rest.starts_with("//") {
            i += rest.find('\n').unwrap_or(rest.len());
        } else if let Some(body) = rest.strip_prefix("/*") {
            i += body.find("*/").map(|p| p + 4).unwrap_or(rest.len());
        } else if parens && c == '(' {
            i += 1;
        } else {
            return i;
        }
    }
}

/// The contents of a '...' or "..." string starting at byte offset `s`.
fn quoted_at(source: &str, s: usize) -> Option<&str> {
    let quote = match source.as_bytes().get(s)? {
        b'"' => '"',
        b'\'' => '\'',
        _ => return None,
    };
    let body = &source[s + 1..];
    body.find(quote).map(|end| &body[..end])
}

pub fn rewrite_cjs_interop(
    source: &str,
    path: &Path,
    interop: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    rewrite_cjs_interop_logged(source, path, interop, &mut |_| {})
}

/// Like `rewrite_cjs_interop`, with a sink for the dev warnings the rewrite
/// cannot fix (Vite logs the same class from `transformCjsImport`).
pub fn rewrite_cjs_interop_logged(
    source: &str,
    path: &Path,
    interop: &dyn Fn(&str) -> Option<String>,
    warn: &mut dyn FnMut(String),
) -> Option<String> {
    let source_type = SourceType::from_path(path).unwrap_or_default();
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return None;
    }

    let mut rewriter = InteropRewriter {
        interop,
        warn,
        path,
        edits: Vec::new(),
        idx: 0,
        needs_ns_helper: false,
    };
    for stmt in &parsed.program.body {
        rewriter.statement(stmt);
    }
    let InteropRewriter {
        mut edits,
        needs_ns_helper,
        ..
    } = rewriter;

    // `import("cjs-dep")`: Vite wraps the promise so the awaited namespace reads
    // like the static-import interop (module.exports on `default`, its
    // properties as named members); otherwise `(await import("dep")).foo` reads
    // off the raw ESM wrapper namespace and is undefined.
    let mut dyn_edits = DynamicImportInterop {
        interop,
        edits: Vec::new(),
    };
    dyn_edits.visit_program(&parsed.program);
    let needs_helper = !dyn_edits.edits.is_empty() || needs_ns_helper;
    edits.extend(dyn_edits.edits);

    if edits.is_empty() {
        return None;
    }
    let mut result = apply_edits(source, edits);
    if needs_helper {
        // After the hashbang, when there is one: a dep entry that doubles as
        // a bin script must keep `#!` at byte 0. The span excludes its newline.
        match parsed
            .program
            .hashbang
            .as_ref()
            .map(|h| h.span.end as usize)
        {
            Some(at) if at > 0 => result.insert_str(at, &format!("\n{DYN_INTEROP_HELPER}")),
            _ => result.insert_str(0, DYN_INTEROP_HELPER),
        }
    }
    Some(result)
}

struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// Applies non-overlapping edits back to front so earlier offsets stay valid.
fn apply_edits(source: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));
    let mut result = source.to_string();
    for e in edits {
        result.replace_range(e.start..e.end, &e.text);
    }
    result
}

/// The top-level statement pass of `rewrite_cjs_interop_logged`. `idx`
/// numbers the `__ojns{n}` / `__ojcjs{n}` bindings in source order.
struct InteropRewriter<'r> {
    interop: &'r dyn Fn(&str) -> Option<String>,
    warn: &'r mut dyn FnMut(String),
    path: &'r Path,
    edits: Vec<Edit>,
    idx: usize,
    needs_ns_helper: bool,
}

impl InteropRewriter<'_> {
    fn next_idx(&mut self) -> usize {
        let n = self.idx;
        self.idx += 1;
        n
    }

    fn push(&mut self, span: Span, text: String) {
        self.edits.push(Edit {
            start: span.start as usize,
            end: span.end as usize,
            text,
        });
    }

    fn statement(&mut self, stmt: &Statement) {
        match stmt {
            Statement::ImportDeclaration(decl) => self.import(decl),
            Statement::ExportFromDeclaration(decl) => self.export_from(decl),
            Statement::ExportAllDeclaration(decl) => self.export_all(decl),
            _ => {}
        }
    }

    fn import(&mut self, decl: &ImportDeclaration) {
        if decl.import_kind.is_type() {
            return;
        }
        let Some(url) = (self.interop)(decl.source.value.as_str()) else {
            return;
        };
        let n = self.next_idx();
        let Some(specs) = &decl.specifiers else {
            self.push(decl.span, format!("import {};", json_str(&url)));
            return;
        };
        let (ns, cjs) = (format!("__ojns{n}"), format!("__ojcjs{n}"));
        let mut out = cjs_import(&ns, &cjs, &url);
        let mut names: Vec<String> = Vec::new();
        for spec in specs {
            match spec {
                ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                    out.push_str(&format!("const {} = {cjs};", s.local.name));
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                    out.push_str(&format!("const {} = {ns};", s.local.name));
                }
                ImportDeclarationSpecifier::ImportSpecifier(s) => {
                    let key = json_key(&export_name(&s.imported));
                    names.push(format!("{key}: {}", s.local.name));
                }
            }
        }
        if !names.is_empty() {
            out.push_str(&format!("const {{ {} }} = {cjs};", names.join(", ")));
        }
        self.push(decl.span, out);
    }

    /// `export { a, b as c } from "cjs"` and `export { default as X } from
    /// "cjs"`; without this the named bindings are undefined (the dep is a
    /// default-only ESM module).
    fn export_from(&mut self, decl: &ExportFromDeclaration) {
        if decl.export_kind.is_type() {
            return;
        }
        let Some(url) = (self.interop)(decl.source.value.as_str()) else {
            return;
        };
        let n = self.next_idx();
        let (ns, cjs) = (format!("__ojns{n}"), format!("__ojcjs{n}"));
        let mut out = cjs_import(&ns, &cjs, &url);
        let value_specs = decl.specifiers.iter().filter(|s| !s.export_kind.is_type());
        for (tmp, spec) in value_specs.enumerate() {
            let local = export_name(&spec.local);
            let value = if local == "default" {
                cjs.clone()
            } else {
                format!("{cjs}[{}]", json_str(&local))
            };
            let exported = json_key(&export_name(&spec.exported));
            let t = format!("__ojex{n}_{tmp}");
            out.push_str(&format!(
                "const {t} = {value};export {{ {t} as {exported} }};"
            ));
        }
        self.push(decl.span, out);
    }

    /// `export * as ns from "cjs"`: consumers must see the interop namespace
    /// (module.exports as `default` plus its properties), the shape the
    /// dynamic-import helper builds, so build it with that helper.
    ///
    /// A bare `export * from "cjs"` is left alone on purpose: ESM has no
    /// dynamic named exports, so a rewrite could only forward statically known
    /// names, which the original statement already re-exports. Runtime-only
    /// names (true UMD) need the dep pre-bundled (optimizeDeps.include), which
    /// is also how Vite covers the shape.
    fn export_all(&mut self, decl: &ExportAllDeclaration) {
        if decl.export_kind.is_type() {
            return;
        }
        let Some(exported) = &decl.exported else {
            // Vite warns here too ("Unable to interop ... may lose module
            // exports").
            if (self.interop)(decl.source.value.as_str()).is_some() {
                (self.warn)(format!(
                    "cannot interop `export * from \"{}\"` in {}; runtime-assigned CommonJS exports are lost through a bare star re-export; use named exports, or pre-bundle the dep (optimizeDeps.include)",
                    decl.source.value,
                    self.path.display(),
                ));
            }
            return;
        };
        let Some(url) = (self.interop)(decl.source.value.as_str()) else {
            return;
        };
        let n = self.next_idx();
        self.needs_ns_helper = true;
        let out = format!(
            "import * as __ojns{n} from {};const __ojex{n} = __oj_dyn_interop(__ojns{n});export {{ __ojex{n} as {} }};",
            json_str(&url),
            json_key(&export_name(exported)),
        );
        self.push(decl.span, out);
    }
}

/// `import * as ns from url` plus `cjs`, the robust CommonJS value (what
/// `require()` returns): oj-wrapped CJS exposes it as `__cjs_exports`,
/// esbuild-prebundled and ESM-with-default expose `default`, a plain ESM
/// namespace is itself. Named imports read off this value, so a barrel that
/// sets `__esModule` without a `default` but keeps its names on
/// module.exports still resolves.
fn cjs_import(ns: &str, cjs: &str, url: &str) -> String {
    format!(
        "import * as {ns} from {};const {cjs} = {ns} && {ns}.__cjs_exports !== undefined ? {ns}.__cjs_exports : ({ns} && {ns}.default !== undefined ? {ns}.default : {ns});",
        json_str(url)
    )
}

/// Vite's `interopNamespace` for a dynamically imported CommonJS dependency: the
/// CJS value becomes `default`, and its own properties the named exports, unless
/// it already is an ES module namespace.
const DYN_INTEROP_HELPER: &str = "const __oj_dyn_interop = (m) => { const v = m && m.__cjs_exports !== undefined ? m.__cjs_exports : (m && m.default !== undefined ? m.default : m); if (v && v.__esModule) return v; return (v && typeof v === \"object\") || typeof v === \"function\" ? Object.assign(Object.create(null), v, { default: v }) : { default: v }; };\n";

struct DynamicImportInterop<'i> {
    interop: &'i dyn Fn(&str) -> Option<String>,
    edits: Vec<Edit>,
}

impl<'a> Visit<'a> for DynamicImportInterop<'_> {
    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        if let Expression::StringLiteral(lit) = &it.source {
            if let Some(url) = (self.interop)(lit.value.as_str()) {
                self.edits.push(Edit {
                    start: it.span.start as usize,
                    end: it.span.end as usize,
                    text: format!("import({}).then(__oj_dyn_interop)", json_str(&url)),
                });
                return;
            }
        }
        walk::walk_import_expression(self, it);
    }
}

fn export_name(n: &ModuleExportName) -> String {
    match n {
        ModuleExportName::IdentifierName(i) => i.name.to_string(),
        ModuleExportName::IdentifierReference(i) => i.name.to_string(),
        ModuleExportName::StringLiteral(l) => l.value.to_string(),
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_key(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
            .unwrap_or(false)
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    {
        s.to_string()
    } else {
        json_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_import_of_a_cjs_dep_is_wrapped_with_the_namespace_interop() {
        let src = "const m = await import(\"cjs-dep\");\nconst n = import(\"esm-dep\");\nimport(`tpl`);\n";
        let out = rewrite_cjs_interop(src, Path::new("a.ts"), &|spec| {
            (spec == "cjs-dep").then(|| "/@oj-deps/cjs-dep.js".to_string())
        })
        .expect("rewritten");
        assert!(out.starts_with("const __oj_dyn_interop = "), "{out}");
        assert!(
            out.contains("import(\"/@oj-deps/cjs-dep.js\").then(__oj_dyn_interop)"),
            "{out}"
        );
        assert!(
            out.contains("import(\"esm-dep\")"),
            "untouched non-interop import: {out}"
        );
        assert!(
            out.contains("import(`tpl`)"),
            "template specifiers are left alone: {out}"
        );
        // Nothing to interop: no rewrite, no helper.
        assert!(rewrite_cjs_interop(src, Path::new("a.ts"), &|_| None).is_none());
    }

    fn interop_all(url: &str) -> impl Fn(&str) -> Option<String> + '_ {
        move |spec: &str| (spec == "cjs-dep").then(|| url.to_string())
    }

    fn run(src: &str) -> String {
        rewrite_cjs_interop(
            src,
            Path::new("m.js"),
            &interop_all("/@oj-deps/cjs-dep.mjs"),
        )
        .unwrap()
    }

    #[test]
    fn default_import_unwraps_esmodule() {
        let out = run(r#"import Foo from "cjs-dep";"#);
        assert!(
            out.contains(r#"import * as __ojns0 from "/@oj-deps/cjs-dep.mjs";"#),
            "{out}"
        );
        assert!(
            out.contains("__ojns0.__cjs_exports !== undefined ? __ojns0.__cjs_exports"),
            "cjs value prefers __cjs_exports: {out}"
        );
        assert!(out.contains("const Foo = __ojcjs0;"), "{out}");
    }

    #[test]
    fn named_imports_destructure_from_module_exports() {
        let out = run(r#"import { a, b as c } from "cjs-dep";"#);
        assert!(out.contains("const { a: a, b: c } = __ojcjs0;"), "{out}");
    }

    #[test]
    fn mixed_default_and_named() {
        let out = run(r#"import D, { x } from "cjs-dep";"#);
        assert!(out.contains("const D = __ojcjs0;"), "{out}");
        assert!(out.contains("const { x: x } = __ojcjs0;"), "{out}");
    }

    #[test]
    fn namespace_import_binds_module_exports() {
        let out = run(r#"import * as ns from "cjs-dep";"#);
        assert!(out.contains("const ns = __ojns0;"), "{out}");
    }

    #[test]
    fn side_effect_import_just_rewrites_specifier() {
        let out = run(r#"import "cjs-dep";"#);
        assert_eq!(out.trim(), r#"import "/@oj-deps/cjs-dep.mjs";"#);
    }

    #[test]
    fn non_interop_and_type_imports_untouched() {
        assert!(rewrite_cjs_interop(
            r#"import x from "other";"#,
            Path::new("m.ts"),
            &interop_all("/u")
        )
        .is_none());
        assert!(rewrite_cjs_interop(
            r#"import type T from "cjs-dep";"#,
            Path::new("m.ts"),
            &interop_all("/u")
        )
        .is_none());
    }

    #[test]
    fn string_named_import() {
        let out = run(r#"import { "weird-name" as w } from "cjs-dep";"#);
        assert!(
            out.contains(r#"const { "weird-name": w } = __ojcjs0;"#),
            "{out}"
        );
    }

    #[test]
    fn reexport_named_from_cjs() {
        let out = run(r#"export { a, b as c } from "cjs-dep";"#);
        assert!(
            out.contains(r#"import * as __ojns0 from "/@oj-deps/cjs-dep.mjs";"#),
            "{out}"
        );
        assert!(
            out.contains(r#"const __ojex0_0 = __ojcjs0["a"];export { __ojex0_0 as a };"#),
            "{out}"
        );
        assert!(
            out.contains(r#"const __ojex0_1 = __ojcjs0["b"];export { __ojex0_1 as c };"#),
            "{out}"
        );
    }

    #[test]
    fn reexport_default_from_cjs_unwraps() {
        let out = run(r#"export { default as X } from "cjs-dep";"#);
        assert!(out.contains("const __ojex0_0 = __ojcjs0;"), "{out}");
        assert!(out.contains("export { __ojex0_0 as X };"), "{out}");
    }

    #[test]
    fn reexport_from_non_interop_untouched() {
        assert!(rewrite_cjs_interop(
            r#"export { a } from "other";"#,
            Path::new("m.js"),
            &interop_all("/u")
        )
        .is_none());
    }

    #[test]
    fn local_export_without_source_untouched() {
        // `export { foo }` with no `from` is a local re-export, not a CJS dep.
        assert!(rewrite_cjs_interop(
            r#"const foo = 1; export { foo };"#,
            Path::new("m.js"),
            &interop_all("/u")
        )
        .is_none());
    }
    #[test]
    fn export_star_as_builds_the_interop_namespace() {
        let out = run(r#"export * as geo from "cjs-dep";"#);
        assert!(
            out.starts_with("const __oj_dyn_interop = "),
            "helper prepended: {out}"
        );
        assert!(
            out.contains(r#"import * as __ojns0 from "/@oj-deps/cjs-dep.mjs";"#),
            "{out}"
        );
        assert!(
            out.contains("const __ojex0 = __oj_dyn_interop(__ojns0);"),
            "{out}"
        );
        assert!(out.contains("export { __ojex0 as geo };"), "{out}");
    }

    #[test]
    fn bare_export_star_is_left_alone() {
        // ESM has no dynamic named exports: a bare star re-export can only
        // forward statically known names, which the unrewritten statement
        // already does. Runtime-only names need pre-bundling (like Vite).
        assert!(rewrite_cjs_interop(
            r#"export * from "cjs-dep";"#,
            Path::new("m.js"),
            &interop_all("/u")
        )
        .is_none());
    }

    #[test]
    fn bare_import_specifiers_finds_every_candidate_position() {
        for (src, want) in [
            (r#"import { a } from "dep";"#, "dep"),
            ("import x from 'dep';", "dep"),
            (r#"export { a } from "dep";"#, "dep"),
            (r#"export * from "dep";"#, "dep"),
            (r#"export * as ns from "dep";"#, "dep"),
            (r#"import "dep";"#, "dep"),
            (r#"const m = await import("dep");"#, "dep"),
            ("import(  'dep')", "dep"),
            // minified: no space between keyword and quote
            (r#"import{a}from"dep";"#, "dep"),
            (r#"import x from"node:path";"#, "node:path"),
            // legal trivia between the keyword and the specifier
            (
                r#"import(/* webpackChunkName: "geo" */ "geodesiclib")"#,
                "geodesiclib",
            ),
            ("import x from // eol\n 'dep';", "dep"),
            (r#"import(("dep"))"#, "dep"),
            ("import x from\u{00a0}'dep';", "dep"),
        ] {
            let specs = bare_import_specifiers(src);
            assert!(
                specs.iter().any(|s| s == want),
                "expected {want} in {specs:?} for: {src}"
            );
        }
        // dedup across positions
        assert_eq!(
            bare_import_specifiers(r#"import a from "dep"; import b from "dep";"#),
            vec!["dep".to_string()]
        );
    }

    #[test]
    fn bare_import_specifiers_skips_relative_only_modules() {
        for src in [
            r#"import { a } from "./sib.js";"#,
            r#"import x from "../up.js";"#,
            r#"export * from "/abs.js";"#,
            r#"import("./dyn.js")"#,
            "const platform_from = \"linux\";",
            r#"const s = "written from ./here";"#,
            "export const a = 1;",
            "",
        ] {
            assert!(
                bare_import_specifiers(src).is_empty(),
                "expected none: {src}"
            );
        }
    }

    #[test]
    fn dyn_interop_helper_lands_after_a_hashbang() {
        let out = rewrite_cjs_interop(
            "#!/usr/bin/env node\nconst m = await import(\"cjs-dep\");\n",
            Path::new("cli.js"),
            &interop_all("/u"),
        )
        .unwrap();
        assert!(
            out.starts_with("#!/usr/bin/env node\nconst __oj_dyn_interop"),
            "{out}"
        );
    }

    #[test]
    fn bare_export_star_warns_but_is_left_alone() {
        let mut warnings = Vec::new();
        let out = rewrite_cjs_interop_logged(
            r#"export * from "cjs-dep";"#,
            Path::new("m.js"),
            &interop_all("/u"),
            &mut |w| warnings.push(w),
        );
        assert!(out.is_none());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("export * from \"cjs-dep\""),
            "{}",
            warnings[0]
        );
        // No warning when the source is not an interop candidate.
        warnings.clear();
        rewrite_cjs_interop_logged(
            r#"export * from "./sib.js";"#,
            Path::new("m.js"),
            &interop_all("/u"),
            &mut |w| warnings.push(w),
        );
        assert!(warnings.is_empty());
    }
}
