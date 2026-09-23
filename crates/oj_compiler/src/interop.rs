// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{ImportDeclarationSpecifier, ModuleExportName, Statement};
use oxc_parser::Parser;
use oxc_span::SourceType;

pub fn rewrite_cjs_interop(
    source: &str,
    path: &Path,
    interop: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let source_type = SourceType::from_path(path).unwrap_or_default();
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return None;
    }

    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut idx = 0usize;

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
    let has_dynamic = !dyn_edits.edits.is_empty();
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

/// Whether `rewrite_cjs_interop` would rewrite this program: some import,
/// export-from, or string-literal dynamic import names a specifier the interop
/// callback maps. Runs on an already-parsed program so the common no-interop
/// module pays no dedicated detection parse; it must mirror the statement
/// shapes `rewrite_cjs_interop` edits exactly.
pub(crate) fn program_wants_interop(
    program: &oxc_ast::ast::Program<'_>,
    interop: &dyn Fn(&str) -> Option<String>,
) -> bool {
    for stmt in &program.body {
        match stmt {
            Statement::ImportDeclaration(decl) => {
                if decl.import_kind.is_type() {
                    continue;
                }
                if interop(decl.source.value.as_str()).is_some() {
                    return true;
                }
            }
            Statement::ExportFromDeclaration(decl) => {
                if decl.export_kind.is_type() {
                    continue;
                }
                if interop(decl.source.value.as_str()).is_some() {
                    return true;
                }
            }
            _ => continue,
        }
    }
    let mut detect = DynamicImportDetect {
        interop,
        found: false,
    };
    {
        use oxc_ast_visit::Visit;
        detect.visit_program(program);
    }
    detect.found
}

struct DynamicImportDetect<'i> {
    interop: &'i dyn Fn(&str) -> Option<String>,
    found: bool,
}

impl<'a> oxc_ast_visit::Visit<'a> for DynamicImportDetect<'_> {
    fn visit_import_expression(&mut self, it: &oxc_ast::ast::ImportExpression<'a>) {
        if self.found {
            return;
        }
        if let oxc_ast::ast::Expression::StringLiteral(lit) = &it.source {
            if (self.interop)(lit.value.as_str()).is_some() {
                self.found = true;
                return;
            }
        }
        oxc_ast_visit::walk::walk_import_expression(self, it);
    }
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
    fn program_wants_interop_mirrors_rewrite() {
        // The folded detection must fire exactly when rewrite_cjs_interop
        // would rewrite, for every statement shape it edits (and none it
        // ignores, like `export * from`).
        let cases = [
            r#"import a from "cjs-dep";"#,
            r#"import { a } from "cjs-dep";"#,
            r#"import * as ns from "cjs-dep";"#,
            r#"import "cjs-dep";"#,
            r#"export { a, b as c } from "cjs-dep";"#,
            r#"export { default as X } from "cjs-dep";"#,
            r#"const m = import("cjs-dep");"#,
            r#"function f() { return import("cjs-dep"); }"#,
            r#"import type T from "cjs-dep";"#,
            r#"export type { T } from "cjs-dep";"#,
            r#"export * from "cjs-dep";"#,
            r#"import x from "other";"#,
            r#"import("other");"#,
            r#"import(`cjs-dep`);"#,
            r#"const foo = 1; export { foo };"#,
        ];
        let interop = interop_all("/@oj-deps/cjs-dep.mjs");
        for src in cases {
            let path = Path::new("m.ts");
            let source_type =
                oxc_span::SourceType::from_path(path).unwrap_or_default();
            let allocator = oxc_allocator::Allocator::default();
            let parsed = oxc_parser::Parser::new(&allocator, src, source_type).parse();
            assert!(!parsed.panicked, "{src}");
            let wants = program_wants_interop(&parsed.program, &interop);
            let rewrites = rewrite_cjs_interop(src, path, &interop).is_some();
            assert_eq!(wants, rewrites, "detection mismatch for {src:?}");
        }
    }

    #[test]
    fn folded_interop_compile_matches_two_step() {
        // compile_module_with_maps_interop must produce byte-identical output
        // to the old flow (rewrite_cjs_interop first, then compile), on both
        // a hit and a miss.
        let opts = crate::CompileOptions::dev();
        let cases = [
            r#"import { a } from "cjs-dep";
console.log(a);
"#,
            r#"const m = await import("cjs-dep");
console.log(m);
"#,
            r#"import x from "other";
console.log(x);
"#,
        ];
        for src in cases {
            let path = Path::new("mod.ts");
            let interop = interop_all("/@oj-deps/cjs-dep.mjs");
            let folded =
                crate::compile_module_with_maps_interop(path, src, &opts, None, &[], Some(&interop))
                    .unwrap();
            let rewritten = rewrite_cjs_interop(src, path, &interop);
            let two_step = crate::compile_module_with_maps(
                path,
                rewritten.as_deref().unwrap_or(src),
                &opts,
                None,
                &[],
            )
            .unwrap();
            assert_eq!(folded.code, two_step.code, "{src}");
            assert_eq!(folded.imports, two_step.imports, "{src}");
            assert_eq!(folded.dynamic_imports, two_step.dynamic_imports, "{src}");
        }
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
}
