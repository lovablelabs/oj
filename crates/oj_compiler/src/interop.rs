// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{ImportDeclarationSpecifier, ModuleExportName, Statement};
use oxc_parser::Parser;
use oxc_span::SourceType;

/// Cheap pre-gate for `rewrite_cjs_interop` on hot paths: can this source
/// possibly import a bare specifier? Scans for a quote opening a non-relative
/// specifier right after `from`, `import` or `import(`. A false positive just
/// runs the parse; the patterns cover every syntactic position an interop
/// candidate can occupy (static import, re-export, side-effect import,
/// dynamic import), so `false` is safe to skip on.
pub fn may_import_bare(source: &str) -> bool {
    let bytes = source.as_bytes();
    let bare_after = |mut i: usize| -> bool {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        // dynamic import: an optional opening paren before the specifier
        if i < bytes.len() && bytes[i] == b'(' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
        }
        if i + 1 >= bytes.len() || (bytes[i] != b'"' && bytes[i] != b'\'') {
            return false;
        }
        !matches!(bytes[i + 1], b'.' | b'/')
    };
    for kw in ["from", "import"] {
        let mut at = 0;
        while let Some(pos) = source[at..].find(kw) {
            let i = at + pos;
            // a keyword, not the tail of an identifier
            let standalone = i == 0
                || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_' && bytes[i - 1] != b'$');
            if standalone && bare_after(i + kw.len()) {
                return true;
            }
            at = i + kw.len();
        }
    }
    false
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

    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut idx = 0usize;
    let mut needs_ns_helper = false;

    for stmt in &parsed.program.body {
        match stmt {
            Statement::ImportDeclaration(decl) => {
                if decl.import_kind.is_type() {
                    continue;
                }
                let Some(url) = interop(decl.source.value.as_str()) else {
                    continue;
                };
                let ns = format!("__ojns{idx}");
                let cjs = format!("__ojcjs{idx}");
                idx += 1;

                // Namespace import + the robust CommonJS value (what `require()`
                // returns): oj-wrapped CJS exposes it as `__cjs_exports`,
                // esbuild-prebundled and ESM-with-default expose `default`, a plain
                // ESM namespace is itself. Named imports read off this value, so a
                // barrel like @sniptt/guards (sets __esModule, so `default` is
                // undefined, but carries its names on module.exports) still resolves.
                let cjs_value = format!(
                    "{ns} && {ns}.__cjs_exports !== undefined ? {ns}.__cjs_exports : ({ns} && {ns}.default !== undefined ? {ns}.default : {ns})",
                );
                let mut out = match &decl.specifiers {
                    None => format!("import {};", json_str(&url)),
                    Some(_) => {
                        format!("import * as {ns} from {};const {cjs} = {cjs_value};", json_str(&url))
                    }
                };
                if let Some(specs) = &decl.specifiers {
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
                                names.push(format!(
                                    "{}: {}",
                                    json_key(&export_name(&s.imported)),
                                    s.local.name
                                ));
                            }
                        }
                    }
                    if !names.is_empty() {
                        out.push_str(&format!("const {{ {} }} = {cjs};", names.join(", ")));
                    }
                }
                edits.push((decl.span.start as usize, decl.span.end as usize, out));
            }
            // Re-exports from a CJS dep: `export { a, b as c } from "cjs"` and
            // `export { default as X } from "cjs"`. Without this the named
            // bindings are undefined (the dep is a default-only ESM module).
            Statement::ExportFromDeclaration(decl) => {
                if decl.export_kind.is_type() {
                    continue;
                }
                let Some(url) = interop(decl.source.value.as_str()) else {
                    continue;
                };
                let n = idx;
                let ns = format!("__ojns{n}");
                let cjs = format!("__ojcjs{n}");
                idx += 1;

                let cjs_value = format!(
                    "{ns} && {ns}.__cjs_exports !== undefined ? {ns}.__cjs_exports : ({ns} && {ns}.default !== undefined ? {ns}.default : {ns})",
                );
                let mut out = format!(
                    "import * as {ns} from {};const {cjs} = {cjs_value};",
                    json_str(&url)
                );
                let mut tmp = 0usize;
                for spec in &decl.specifiers {
                    if spec.export_kind.is_type() {
                        continue;
                    }
                    let local = export_name(&spec.local);
                    let exported = export_name(&spec.exported);
                    let value = if local == "default" {
                        cjs.clone()
                    } else {
                        format!("{cjs}[{}]", json_str(&local))
                    };
                    let t = format!("__ojex{n}_{tmp}");
                    tmp += 1;
                    out.push_str(&format!("const {t} = {value};"));
                    out.push_str(&format!("export {{ {t} as {} }};", json_key(&exported)));
                }
                edits.push((decl.span.start as usize, decl.span.end as usize, out));
            }
            // `export * as ns from "cjs"`: the namespace consumers see must be
            // the interop namespace (module.exports as `default` plus its
            // properties as members), the same shape the dynamic-import helper
            // builds — so build it with that helper.
            //
            // A bare `export * from "cjs"` is left alone on purpose: ESM has
            // no dynamic named exports, so a rewrite could only forward names
            // known statically — exactly what the un-rewritten statement
            // already re-exports from the compiled dep. Runtime-only names
            // (true UMD) through a star barrel need the dep pre-bundled
            // (optimizeDeps.include), which is also how Vite covers the shape.
            Statement::ExportAllDeclaration(decl) => {
                if decl.export_kind.is_type() {
                    continue;
                }
                let Some(exported) = &decl.exported else {
                    // Vite warns here too ("Unable to interop ... may lose
                    // module exports"): runtime-assigned names cannot ride a
                    // bare star re-export.
                    if interop(decl.source.value.as_str()).is_some() {
                        warn(format!(
                            "cannot interop `export * from \"{}\"` in {}; runtime-assigned CommonJS exports are lost through a bare star re-export — use named exports, or pre-bundle the dep (optimizeDeps.include)",
                            decl.source.value,
                            path.display(),
                        ));
                    }
                    continue;
                };
                let Some(url) = interop(decl.source.value.as_str()) else {
                    continue;
                };
                let n = idx;
                let ns = format!("__ojns{n}");
                idx += 1;
                needs_ns_helper = true;
                let out = format!(
                    "import * as {ns} from {};const __ojex{n} = __oj_dyn_interop({ns});export {{ __ojex{n} as {} }};",
                    json_str(&url),
                    json_key(&export_name(exported)),
                );
                edits.push((decl.span.start as usize, decl.span.end as usize, out));
            }
            _ => continue,
        }
    }

    // `import("cjs-dep")`: Vite wraps the promise so the awaited namespace reads
    // like the static-import interop above (module.exports on `default`, its
    // properties as named members). Without it `(await import("dep")).foo` reads
    // off the raw ESM wrapper namespace and is undefined.
    let mut dyn_edits = DynamicImportInterop {
        interop,
        edits: Vec::new(),
    };
    {
        use oxc_ast_visit::Visit;
        dyn_edits.visit_program(&parsed.program);
    }
    let has_dynamic = !dyn_edits.edits.is_empty() || needs_ns_helper;
    edits.extend(dyn_edits.edits);

    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut result = source.to_string();
    for (start, end, text) in edits {
        result.replace_range(start..end, &text);
    }
    if has_dynamic {
        result.insert_str(0, DYN_INTEROP_HELPER);
    }
    Some(result)
}

/// Vite's `interopNamespace` for a dynamically imported CommonJS dependency: the
/// CJS value becomes `default`, and its own properties the named exports, unless
/// it already is an ES module namespace.
const DYN_INTEROP_HELPER: &str = "const __oj_dyn_interop = (m) => { const v = m && m.__cjs_exports !== undefined ? m.__cjs_exports : (m && m.default !== undefined ? m.default : m); if (v && v.__esModule) return v; return (v && typeof v === \"object\") || typeof v === \"function\" ? Object.assign(Object.create(null), v, { default: v }) : { default: v }; };\n";

struct DynamicImportInterop<'i> {
    interop: &'i dyn Fn(&str) -> Option<String>,
    edits: Vec<(usize, usize, String)>,
}

impl<'a> oxc_ast_visit::Visit<'a> for DynamicImportInterop<'_> {
    fn visit_import_expression(&mut self, it: &oxc_ast::ast::ImportExpression<'a>) {
        if let oxc_ast::ast::Expression::StringLiteral(lit) = &it.source {
            if let Some(url) = (self.interop)(lit.value.as_str()) {
                self.edits.push((
                    it.span.start as usize,
                    it.span.end as usize,
                    format!("import({}).then(__oj_dyn_interop)", json_str(&url)),
                ));
                return;
            }
        }
        oxc_ast_visit::walk::walk_import_expression(self, it);
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
        assert!(out.contains("import(\"/@oj-deps/cjs-dep.js\").then(__oj_dyn_interop)"), "{out}");
        assert!(out.contains("import(\"esm-dep\")"), "untouched non-interop import: {out}");
        assert!(out.contains("import(`tpl`)"), "template specifiers are left alone: {out}");
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
        assert!(out.starts_with("const __oj_dyn_interop = "), "helper prepended: {out}");
        assert!(
            out.contains(r#"import * as __ojns0 from "/@oj-deps/cjs-dep.mjs";"#),
            "{out}"
        );
        assert!(out.contains("const __ojex0 = __oj_dyn_interop(__ojns0);"), "{out}");
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
    fn may_import_bare_finds_every_candidate_position() {
        for src in [
            r#"import { a } from "dep";"#,
            "import x from 'dep';",
            r#"export { a } from "dep";"#,
            r#"export * from "dep";"#,
            r#"export * as ns from "dep";"#,
            r#"import "dep";"#,
            r#"const m = await import("dep");"#,
            "import(  'dep')",
            // minified: no space between keyword and quote
            r#"import{a}from"dep";"#,
            r#"import x from"node:path";"#,
        ] {
            assert!(may_import_bare(src), "expected candidate: {src}");
        }
    }

    #[test]
    fn may_import_bare_skips_relative_only_modules() {
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
            assert!(!may_import_bare(src), "expected no candidate: {src}");
        }
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
        assert!(warnings[0].contains("export * from \"cjs-dep\""), "{}", warnings[0]);
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
