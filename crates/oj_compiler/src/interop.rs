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

/// How Node reads the importing file, which decides Vite's interop of a
/// CommonJS dep flagged `__esModule` (importAnalysis.ts, `isNodeMode` and
/// `isNodeModeForDynamicImport`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImporterFormat {
    /// Vite's `isFilePathESM`: an `.mjs`/`.mts` file, or one under a nearest
    /// package.json with `"type": "module"`. A namespace import is then
    /// module.exports beside `default`, flagged or not.
    pub esm: bool,
    /// Vite's `isFilePathFormatExplicit`: also a `.cjs`/`.cts` file or one
    /// under `"type": "commonjs"`. The same, for a dynamic import.
    pub explicit: bool,
}

impl ImporterFormat {
    /// An ES module by its own extension or package.
    pub const ESM: Self = Self {
        esm: true,
        explicit: true,
    };
}

pub fn rewrite_cjs_interop(
    source: &str,
    path: &Path,
    interop: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    rewrite_cjs_interop_logged(source, path, ImporterFormat::ESM, interop, &mut |_| {})
}

/// Like `rewrite_cjs_interop`, with a sink for the dev warnings the rewrite
/// cannot fix (Vite logs the same class from `transformCjsImport`).
pub fn rewrite_cjs_interop_logged(
    source: &str,
    path: &Path,
    format: ImporterFormat,
    interop: &dyn Fn(&str) -> Option<String>,
    warn: &mut dyn FnMut(String),
) -> Option<String> {
    let source_type = SourceType::from_path(path).unwrap_or_default();
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.fatal_error {
        return None;
    }

    let mut rewriter = InteropRewriter {
        interop,
        warn,
        path,
        source,
        format,
        edits: Vec::new(),
        hoisted: String::new(),
        idx: 0,
        needs_ns_helper: false,
    };
    for stmt in &parsed.program.body {
        rewriter.statement(stmt);
    }
    let InteropRewriter {
        mut edits,
        hoisted,
        needs_ns_helper,
        ..
    } = rewriter;

    // `import("cjs-dep")`: Vite wraps the promise so the awaited namespace reads
    // like the static-import interop (module.exports on `default`, its
    // properties as named members); otherwise `(await import("dep")).foo` reads
    // off the raw ESM wrapper namespace and is undefined.
    let mut dyn_edits = DynamicImportInterop {
        interop,
        source,
        node_mode: format.explicit,
        edits: Vec::new(),
    };
    dyn_edits.visit_program(&parsed.program);
    let needs_helper = !dyn_edits.edits.is_empty() || needs_ns_helper;
    edits.extend(dyn_edits.edits);

    if edits.is_empty() {
        return None;
    }
    let mut result = apply_edits(source, edits);
    // Vite hoists the bindings to the start of the file (interopNamedImports,
    // `appendLeft(getFileStartIndex(source))`), so a function declared in
    // the module and called above the import statement finds them
    // initialised. Here they go before the first statement, on its line:
    // past a hashbang, the directives and the leading comments, which keep
    // their place, and without adding a line. The helper comes first, as
    // the bindings call it. Every edit starts at or after that statement, so
    // its offset in the result is its offset in the source.
    let head = if needs_helper {
        format!("{DYN_INTEROP_HELPER}{hoisted}")
    } else {
        hoisted
    };
    if let Some(first) = parsed.program.body.first() {
        result.insert_str(oxc_span::GetSpan::span(first).start as usize, &head);
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

/// The line breaks of a replaced span, appended to its one-line replacement
/// so every later line keeps its number (Vite's `getLineBreaks`).
fn line_breaks(text: &str) -> String {
    "\n".repeat(text.matches('\n').count())
}

/// The top-level statement pass of `rewrite_cjs_interop_logged`. `idx`
/// numbers the `__ojns{n}` / `__ojcjs{n}` bindings in source order. A
/// statement is replaced in place by its import alone; the bindings read off
/// it collect in `hoisted`.
struct InteropRewriter<'r> {
    interop: &'r dyn Fn(&str) -> Option<String>,
    warn: &'r mut dyn FnMut(String),
    path: &'r Path,
    source: &'r str,
    format: ImporterFormat,
    edits: Vec<Edit>,
    hoisted: String,
    idx: usize,
    needs_ns_helper: bool,
}

impl InteropRewriter<'_> {
    fn next_idx(&mut self) -> usize {
        let n = self.idx;
        self.idx += 1;
        n
    }

    /// Replaces the statement at `span` with `text` on one line.
    fn push(&mut self, span: Span, text: String) {
        let (start, end) = (span.start as usize, span.end as usize);
        let text = text + &line_breaks(&self.source[start..end]);
        self.edits.push(Edit { start, end, text });
    }

    fn statement(&mut self, stmt: &Statement) {
        match stmt {
            Statement::ImportDeclaration(decl) => self.import(decl),
            Statement::ExportFromDeclaration(decl) => self.export_from(decl),
            Statement::ExportAllDeclaration(decl) => self.export_all(decl),
            _ => {}
        }
    }

    /// Vite's default import of a dep that needs interop (transformCjsImport):
    /// module.exports for an importer that is an ES module to Node, else the
    /// `default` of one flagged `__esModule`.
    fn default_of(&self, cjs: &str) -> String {
        if self.format.esm {
            cjs.to_string()
        } else {
            format!("({cjs} && {cjs}.__esModule ? {cjs}.default : {cjs})")
        }
    }

    fn import(&mut self, decl: &ImportDeclaration) {
        if decl.import_kind.is_type() {
            return;
        }
        let Some(url) = (self.interop)(crate::lit_str(decl.source.value)) else {
            return;
        };
        let n = self.next_idx();
        let Some(specs) = &decl.specifiers else {
            self.push(decl.span, format!("import {};", json_str(&url)));
            return;
        };
        let (ns, cjs) = (format!("__ojns{n}"), format!("__ojcjs{n}"));
        let mut out = cjs_value(&ns, &cjs);
        let mut names: Vec<String> = Vec::new();
        for spec in specs {
            match spec {
                ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                    out.push_str(&format!(
                        "const {} = {};",
                        s.local.name,
                        self.default_of(&cjs)
                    ));
                }
                // Vite's transformCjsImport: the namespace of a CommonJS dep
                // is module.exports spread beside `default`, so a name the
                // dep only assigns at runtime (a UMD wrapper) is a member.
                // The helper a dynamic import goes through builds it, so
                // both forms of one dep have the same members.
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                    self.needs_ns_helper = true;
                    out.push_str(&format!(
                        "const {} = __oj_dyn_interop({ns}, {});",
                        s.local.name,
                        u8::from(self.format.esm)
                    ));
                }
                ImportDeclarationSpecifier::ImportSpecifier(s) => {
                    let imported = export_name(&s.imported);
                    if imported == "default" {
                        out.push_str(&format!(
                            "const {} = {};",
                            s.local.name,
                            self.default_of(&cjs)
                        ));
                    } else {
                        names.push(format!("{}: {}", json_key(&imported), s.local.name));
                    }
                }
            }
        }
        if !names.is_empty() {
            out.push_str(&format!("const {{ {} }} = {cjs};", names.join(", ")));
        }
        self.hoisted.push_str(&out);
        self.push(
            decl.span,
            format!("import * as {ns} from {};", json_str(&url)),
        );
    }

    /// `export { a, b as c } from "cjs"` and `export { default as X } from
    /// "cjs"`; without this the named bindings are undefined (the dep is a
    /// default-only ESM module).
    fn export_from(&mut self, decl: &ExportFromDeclaration) {
        if decl.export_kind.is_type() {
            return;
        }
        let Some(url) = (self.interop)(crate::lit_str(decl.source.value)) else {
            return;
        };
        let n = self.next_idx();
        let (ns, cjs) = (format!("__ojns{n}"), format!("__ojcjs{n}"));
        let mut out = cjs_value(&ns, &cjs);
        let value_specs = decl.specifiers.iter().filter(|s| !s.export_kind.is_type());
        for (tmp, spec) in value_specs.enumerate() {
            let local = export_name(&spec.local);
            let value = if local == "default" {
                self.default_of(&cjs)
            } else {
                format!("{cjs}[{}]", json_str(&local))
            };
            let exported = json_key(&export_name(&spec.exported));
            let t = format!("__ojex{n}_{tmp}");
            out.push_str(&format!(
                "const {t} = {value};export {{ {t} as {exported} }};"
            ));
        }
        self.hoisted.push_str(&out);
        self.push(
            decl.span,
            format!("import * as {ns} from {};", json_str(&url)),
        );
    }

    /// `export * as ns from "cjs"`: Vite's transformCjsImport returns nothing
    /// for an ExportAllDeclaration, so only the URL is rewritten and `ns` is
    /// the ES namespace of the pre-bundle (its lone `default`). The same here.
    ///
    /// A bare `export * from "cjs"` is left alone and warns, as Vite's does:
    /// ESM has no dynamic named exports, so runtime-assigned names are lost
    /// through it.
    fn export_all(&mut self, decl: &ExportAllDeclaration) {
        if decl.export_kind.is_type() {
            return;
        }
        let Some(url) = (self.interop)(crate::lit_str(decl.source.value)) else {
            return;
        };
        if decl.exported.is_some() {
            self.next_idx();
            self.push(decl.source.span, json_str(&url));
            return;
        }
        // Vite warns here too ("Unable to interop ... may lose module
        // exports").
        (self.warn)(format!(
            "cannot interop `export * from \"{}\"` in {}; runtime-assigned CommonJS exports are lost through a bare star re-export; use named exports, or pre-bundle the dep (optimizeDeps.include)",
            decl.source.value.display(),
            self.path.display(),
        ));
    }
}

/// `cjs`, the CommonJS value (what `require()` returns) of the module
/// imported as the namespace `ns`: oj-wrapped CJS exposes it as
/// `__cjs_exports`, a pre-bundle as `default` (what Vite's rewrite imports),
/// and a module with neither is read as it is, where Vite's default import
/// would fail to link.
fn cjs_value(ns: &str, cjs: &str) -> String {
    format!(
        "const {cjs} = {ns} && {ns}.__cjs_exports !== undefined ? {ns}.__cjs_exports : ({ns} && {ns}.default !== undefined ? {ns}.default : {ns});"
    )
}

/// Vite's `interopHelper` (importAnalysis), for a namespace or dynamic
/// import of a dependency that needs interop: its CommonJS value (see
/// [`cjs_value`]) becomes `default` and that value's own enumerable
/// properties the other members (none for an array or a primitive). With `n`
/// unset (the importer is not an ES module to Node, see [`ImporterFormat`]) a
/// value flagged `__esModule` is the namespace as it is, so its `default` is
/// the package's own.
///
/// The helper sits in the module's own scope, where a top-level binding may
/// be called `Array` (`import { Array } from "effect"`), so it reaches
/// `isArray` through a literal's constructor and names no global.
const DYN_INTEROP_HELPER: &str = "const __oj_dyn_interop = (m, n) => { const v = m && m.__cjs_exports !== undefined ? m.__cjs_exports : (m && m.default !== undefined ? m.default : m); return n || !(v && v.__esModule) ? { ...((typeof v === \"object\" && ![].constructor.isArray(v)) || typeof v === \"function\" ? v : {}), default: v } : v; };";

struct DynamicImportInterop<'i> {
    interop: &'i dyn Fn(&str) -> Option<String>,
    source: &'i str,
    /// Vite's `isNodeModeForDynamicImport`.
    node_mode: bool,
    edits: Vec<Edit>,
}

impl<'a> Visit<'a> for DynamicImportInterop<'_> {
    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        if let Expression::StringLiteral(lit) = &it.source {
            if let Some(url) = (self.interop)(crate::lit_str(lit.value)) {
                let (start, end) = (it.span.start as usize, it.span.end as usize);
                self.edits.push(Edit {
                    start,
                    end,
                    text: format!(
                        "import({}).then((m) => __oj_dyn_interop(m, {})){}",
                        json_str(&url),
                        u8::from(self.node_mode),
                        line_breaks(&self.source[start..end])
                    ),
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
        ModuleExportName::StringLiteral(l) => crate::lit_str(l.value).to_string(),
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
            out.contains("import(\"/@oj-deps/cjs-dep.js\").then((m) => __oj_dyn_interop(m, 1))"),
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
        assert!(
            out.starts_with("const __oj_dyn_interop = "),
            "helper prepended: {out}"
        );
        assert!(
            out.contains("const ns = __oj_dyn_interop(__ojns0, 1);"),
            "{out}"
        );
    }

    /// Vite's `isNodeMode` for the namespace and `isNodeModeForDynamicImport`
    /// for `import()`; `export * as` is not rewritten.
    #[test]
    fn the_importer_format_sets_the_helper_flag_per_form() {
        let src = "import * as ns from \"cjs-dep\";\nexport * as re from \"cjs-dep\";\nconst m = import(\"cjs-dep\");\n";
        let run = |esm, explicit| {
            rewrite_cjs_interop_logged(
                src,
                Path::new("m.js"),
                ImporterFormat { esm, explicit },
                &interop_all("/u"),
                &mut |_| {},
            )
            .unwrap()
        };
        // No `type` in the package: neither form is in Node mode.
        let out = run(false, false);
        assert!(
            out.contains("const ns = __oj_dyn_interop(__ojns0, 0);"),
            "{out}"
        );
        assert!(out.contains("export * as re from \"/u\";"), "{out}");
        assert!(
            out.contains("import(\"/u\").then((m) => __oj_dyn_interop(m, 0))"),
            "{out}"
        );
        // `"type": "commonjs"`: explicit, so the dynamic form alone is.
        let out = run(false, true);
        assert!(
            out.contains("const ns = __oj_dyn_interop(__ojns0, 0);"),
            "{out}"
        );
        assert!(
            out.contains("import(\"/u\").then((m) => __oj_dyn_interop(m, 1))"),
            "{out}"
        );
        let out = run(true, true);
        assert!(
            out.contains("const ns = __oj_dyn_interop(__ojns0, 1);"),
            "{out}"
        );
        assert!(
            out.contains("import(\"/u\").then((m) => __oj_dyn_interop(m, 1))"),
            "{out}"
        );
    }

    /// The helper names no global a module could shadow, and adds no line.
    #[test]
    fn interop_helper_names_no_shadowable_global() {
        for global in ["Object.", "Array.", "Reflect.", "globalThis"] {
            assert!(
                !DYN_INTEROP_HELPER.contains(global),
                "{global} in {DYN_INTEROP_HELPER}"
            );
        }
        assert!(!DYN_INTEROP_HELPER.contains('\n'));
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
    /// Vite's transformCjsImport returns nothing for `export * as`, so only
    /// the URL is rewritten: no helper, no binding.
    #[test]
    fn export_star_as_only_rewrites_the_url() {
        let out = run("export * as geo from\n  \"cjs-dep\";\nmarker();\n");
        assert_eq!(
            out,
            "export * as geo from\n  \"/@oj-deps/cjs-dep.mjs\";\nmarker();\n"
        );
    }

    /// The default member in each of its forms, by the importer (Vite's
    /// transformCjsImport: `isNodeMode ? m : !m.__esModule ? m : m.default`).
    #[test]
    fn default_forms_follow_the_importer_format() {
        let run = |src: &str, esm| {
            rewrite_cjs_interop_logged(
                src,
                Path::new("m.js"),
                ImporterFormat { esm, explicit: esm },
                &interop_all("/u"),
                &mut |_| {},
            )
            .unwrap()
        };
        let own = "(__ojcjs0 && __ojcjs0.__esModule ? __ojcjs0.default : __ojcjs0)";
        for (src, binding) in [
            (r#"import d from "cjs-dep";"#, "const d = "),
            (r#"import d, { a } from "cjs-dep";"#, "const d = "),
            (r#"import { default as d } from "cjs-dep";"#, "const d = "),
            (
                r#"export { default } from "cjs-dep";"#,
                "const __ojex0_0 = ",
            ),
            (
                r#"export { default as d } from "cjs-dep";"#,
                "const __ojex0_0 = ",
            ),
        ] {
            let out = run(src, true);
            assert!(out.contains(&format!("{binding}__ojcjs0;")), "{src}: {out}");
            let out = run(src, false);
            assert!(out.contains(&format!("{binding}{own};")), "{src}: {out}");
        }
        // A named member is read off module.exports either way.
        let out = run(r#"import d, { a } from "cjs-dep";"#, false);
        assert!(out.contains("const { a: a } = __ojcjs0;"), "{out}");
        let out = run(r#"export { a as default } from "cjs-dep";"#, false);
        assert!(out.contains(r#"const __ojex0_0 = __ojcjs0["a"];"#), "{out}");
    }

    /// The bindings are hoisted before the first statement, on its line, in
    /// source order after the helper; each statement keeps its line.
    #[test]
    fn bindings_are_hoisted_and_lines_keep_their_numbers() {
        let src = "#!/usr/bin/env node\n\"use client\";\n// leading\ninit();\nimport {\n  a,\n  b as c,\n} from \"cjs-dep\";\nimport * as ns from \"cjs-dep\";\nconst m = await import(\n  \"cjs-dep\"\n);\nexport {\n  x,\n} from \"cjs-dep\";\nfunction init() { return ns.x; }\nmarker();\n";
        let out = run(src);
        let (before, after): (Vec<&str>, Vec<&str>) =
            (src.lines().collect(), out.lines().collect());
        assert_eq!(before.len(), after.len(), "{out}");
        // Untouched lines are where they were.
        for (i, line) in before.iter().enumerate() {
            if [
                "#!",
                "\"use client\"",
                "// leading",
                "function init",
                "marker()",
            ]
            .iter()
            .any(|p| line.starts_with(p))
            {
                assert_eq!(after[i], *line, "line {}: {out}", i + 1);
            }
        }
        let head = after[3];
        assert!(head.ends_with("init();"), "{head}");
        let at = |needle: &str| {
            head.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {head}"))
        };
        assert_eq!(at("const __oj_dyn_interop = "), 0);
        assert!(at("const __oj_dyn_interop = ") < at("const __ojcjs0 = "));
        assert!(
            at("const { a: a, b: c } = __ojcjs0;") < at("const ns = __oj_dyn_interop(__ojns1, 1);")
        );
        assert!(at("const ns = ") < at("export { __ojex2_0 as x };"));
        assert!(at("export { __ojex2_0 as x };") < at("init();"));
        assert_eq!(
            after[4],
            r#"import * as __ojns0 from "/@oj-deps/cjs-dep.mjs";"#
        );
        assert_eq!(
            after[8],
            r#"import * as __ojns1 from "/@oj-deps/cjs-dep.mjs";"#
        );
        assert!(after[9].starts_with("const m = await import("), "{out}");
        assert_eq!(
            after[12],
            r#"import * as __ojns2 from "/@oj-deps/cjs-dep.mjs";"#
        );
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
            ImporterFormat::ESM,
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
            ImporterFormat::ESM,
            &interop_all("/u"),
            &mut |w| warnings.push(w),
        );
        assert!(warnings.is_empty());
    }
}
