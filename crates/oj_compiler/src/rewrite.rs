use crate::bundle;
use crate::compile::ImportRewriter;
use oxc_allocator::Allocator;
use oxc_allocator::TakeIn;
use oxc_ast::ast::{
    Argument, Expression, ImportDeclarationSpecifier, ImportExpression, Program, Statement,
    StringLiteral,
};
use oxc_ast_visit::{walk_mut, VisitMut};
use oxc_span::{GetSpan, SPAN};
use std::collections::HashMap;

pub(crate) fn rewrite_module_specifiers_pub<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    rewriter: &mut ImportRewriter,
) -> (Vec<String>, Vec<String>) {
    let mut opt: Option<&mut ImportRewriter> = Some(rewriter);
    let specifiers = rewrite_module_specifiers(allocator, program, &mut opt, true);
    (specifiers.imports, specifiers.dynamic_imports)
}

/// What [`rewrite_module_specifiers`] collected, specifiers already rewritten.
pub(crate) struct ModuleSpecifiers {
    pub imports: Vec<String>,
    pub dynamic_imports: Vec<String>,
    /// Per specifier, the binding names the module uses from it (Vite's
    /// `importedBindings`), in first-seen order.
    pub bindings: Vec<(String, Vec<String>)>,
}

/// Rewrites static import/export-from sources (and, with a rewriter, dynamic
/// `import("...")` literals) and collects them. `served` says the output is
/// what a dev loader imports (the browser, the SSR runner), where a static
/// import's attributes go (see [`drop_import_attributes`]).
pub(crate) fn rewrite_module_specifiers<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    rewriter: &mut Option<&mut ImportRewriter>,
    served: bool,
) -> ModuleSpecifiers {
    let mut imports = Vec::new();
    let mut dynamic_imports = Vec::new();
    let mut bindings = BindingsBuilder::default();
    for stmt in program.body.iter_mut() {
        let names = imported_names(stmt);
        if served {
            drop_import_attributes(stmt);
        }
        let Some(lit) = static_source(stmt) else {
            continue;
        };
        if let Some(rewriter) = rewriter.as_deref_mut() {
            rewrite_literal(allocator, lit, rewriter);
        }
        let spec = crate::lit_str(lit.value).to_string();
        bindings.extend(&spec, names);
        imports.push(spec);
    }
    if let Some(rewriter) = rewriter.as_deref_mut() {
        DynamicImportRewriter {
            allocator,
            rewriter,
            dynamic: &mut dynamic_imports,
        }
        .visit_program(program);
    }
    for spec in &dynamic_imports {
        bindings.extend(spec, vec!["*".to_string()]);
    }
    ModuleSpecifiers {
        imports,
        dynamic_imports,
        bindings: bindings.list,
    }
}

/// Swaps a string literal's value for the rewriter's answer, if any.
pub(crate) fn rewrite_literal<'a>(
    allocator: &'a Allocator,
    lit: &mut StringLiteral<'a>,
    rewriter: &mut ImportRewriter,
) {
    if let Some(new_spec) = rewriter(crate::lit_str(lit.value)) {
        lit.value = allocator.alloc_str(&new_spec).into();
        lit.raw = None;
    }
}

/// The names a statement pulls from its source, for partial-accept gating: a
/// side-effect import records an empty list (always within any accepted set),
/// a namespace import or star re-export records `*` (never within one).
fn imported_names(stmt: &Statement) -> Vec<String> {
    match stmt {
        Statement::ImportDeclaration(decl) => decl
            .specifiers
            .iter()
            .flatten()
            .map(|s| match s {
                ImportDeclarationSpecifier::ImportSpecifier(s) => s.imported.name().to_string(),
                ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => "default".to_string(),
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(_) => "*".to_string(),
            })
            .collect(),
        Statement::ExportFromDeclaration(decl) => decl
            .specifiers
            .iter()
            .map(|s| bundle::export_name(&s.local))
            .collect(),
        Statement::ExportAllDeclaration(_) => vec!["*".to_string()],
        _ => Vec::new(),
    }
}

/// Vite's importAnalysis drops a static import's attributes
/// (`with { type: "json" }`) from the code it serves: the module it answers
/// is JavaScript whatever the file (a JSON file's is `export default {...}`),
/// and a browser holding the attribute would reject it as no JSON module.
/// The same holds for the server: Vite's ssrTransform replaces each static
/// import with a `__vite_ssr_import__` call (`defineImport`), so no
/// attribute reaches its module runner, and the json plugin's module is what
/// it gets. oj's SSR engine keeps imports as ES syntax and does honour the
/// attribute: it asks for a JSON module and the JavaScript module the host
/// serves is no match ("Cannot resolve module ./recipe.json"). A dynamic
/// import keeps its options: Vite's importAnalysis strips a static import's
/// attributes only, and its ssrTransform renames `import(` to
/// `__vite_ssr_dynamic_import__(`, whose runner takes the specifier alone;
/// oj's runner loads a dynamic import by its specifier too.
fn drop_import_attributes(stmt: &mut Statement) {
    match stmt {
        Statement::ImportDeclaration(decl) => decl.with_clause = None,
        Statement::ExportFromDeclaration(decl) => decl.with_clause = None,
        Statement::ExportAllDeclaration(decl) => decl.with_clause = None,
        _ => {}
    }
}

fn static_source<'s, 'a>(stmt: &'s mut Statement<'a>) -> Option<&'s mut StringLiteral<'a>> {
    match stmt {
        Statement::ImportDeclaration(decl) => Some(&mut decl.source),
        Statement::ExportFromDeclaration(decl) => Some(&mut decl.source),
        Statement::ExportAllDeclaration(decl) => Some(&mut decl.source),
        _ => None,
    }
}

/// Ordered per-specifier binding lists with a map index, so repeated imports
/// of one specifier merge without a linear scan.
#[derive(Default)]
struct BindingsBuilder {
    list: Vec<(String, Vec<String>)>,
    index: HashMap<String, usize>,
}

impl BindingsBuilder {
    fn extend(&mut self, spec: &str, names: Vec<String>) {
        match self.index.get(spec) {
            Some(&i) => self.list[i].1.extend(names),
            None => {
                self.index.insert(spec.to_string(), self.list.len());
                self.list.push((spec.to_string(), names));
            }
        }
    }
}

struct DynamicImportRewriter<'a, 'b> {
    allocator: &'a Allocator,
    rewriter: &'b mut ImportRewriter<'b>,
    dynamic: &'b mut Vec<String>,
}

impl<'a> VisitMut<'a> for DynamicImportRewriter<'a, '_> {
    fn visit_import_expression(&mut self, it: &mut ImportExpression<'a>) {
        // A template literal without a substitution is a literal specifier
        // (Vite's importAnalysis, `templateLiteralRE`): it becomes the string
        // it spells, then is rewritten like one.
        if let Expression::TemplateLiteral(tpl) = &it.source {
            let cooked = tpl.quasis.first().and_then(|q| q.value.cooked);
            if let (true, Some(cooked)) = (tpl.expressions.is_empty(), cooked) {
                it.source = Expression::new_string_literal(
                    tpl.span,
                    cooked,
                    None,
                    &oxc_ast::builder::AstBuilder::new(self.allocator),
                );
            }
        }
        if let Expression::StringLiteral(lit) = &mut it.source {
            rewrite_literal(self.allocator, lit, self.rewriter);
            self.dynamic.push(crate::lit_str(lit.value).to_string());
        }
        walk_mut::walk_import_expression(self, it);
    }
}

/// Vite's `isExplicitImportRequired`: neither a script (`isJSRequest`: knownJsSrcRE, or
/// no extension and no trailing `/`) nor a stylesheet (`CSS_LANGS_RE`).
pub fn explicit_import_required(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = match name.rfind('.') {
        Some(dot) if dot > 0 => &name[dot + 1..],
        _ => "",
    };
    const JS: &[&str] = &[
        "js", "jsx", "ts", "tsx", "mjs", "mts", "vue", "marko", "svelte", "astro", "imba", "mdx",
    ];
    const CSS: &[&str] = &[
        "css", "less", "sass", "scss", "styl", "stylus", "pcss", "postcss", "sss",
    ];
    if ext.is_empty() {
        return path.ends_with('/');
    }
    !JS.contains(&ext) && !CSS.contains(&ext)
}

/// Wraps a computed `import(x)` as `import(__vite__injectQuery(x, 'import'))`, as Vite's
/// importAnalysis does on the client; returns whether any import was wrapped.
pub(crate) fn inject_query_into_dynamic_imports<'a>(
    allocator: &'a Allocator,
    source_text: &str,
    program: &mut Program<'a>,
) -> bool {
    let mut injector = InjectQuery {
        allocator,
        source_text,
        wrapped: false,
    };
    injector.visit_program(program);
    injector.wrapped
}

struct InjectQuery<'a, 's> {
    allocator: &'a Allocator,
    source_text: &'s str,
    wrapped: bool,
}

impl InjectQuery<'_, '_> {
    /// Wrap unless the text is quoted at both ends with no line break (`urlIsStringRE`)
    /// and what it quotes names a script or a stylesheet.
    fn wraps(&self, arg: &Expression) -> bool {
        let span = arg.span();
        let Some(raw) = self.source_text.get(span.start as usize..span.end as usize) else {
            return true;
        };
        let quoted = raw.len() >= 2
            && matches!(raw.as_bytes()[0], b'\'' | b'"' | b'`')
            && raw.as_bytes()[raw.len() - 1] == raw.as_bytes()[0]
            && !raw.contains(['\n', '\r', '\u{2028}', '\u{2029}']);
        !quoted || explicit_import_required(&raw[1..raw.len() - 1])
    }
}

impl<'a> VisitMut<'a> for InjectQuery<'a, '_> {
    fn visit_import_expression(&mut self, it: &mut ImportExpression<'a>) {
        walk_mut::walk_import_expression(self, it);
        // A literal is read and rewritten already, marked where it needs it.
        let wrap = match &it.source {
            Expression::StringLiteral(_) => false,
            Expression::TemplateLiteral(tpl) if tpl.expressions.is_empty() => false,
            arg => self.wraps(arg),
        };
        if !wrap {
            return;
        }
        let builder = oxc_ast::builder::AstBuilder::new(self.allocator);
        let span = it.source.span();
        let url = it.source.take_in(&self.allocator);
        let callee = Expression::new_identifier(SPAN, "__vite__injectQuery", &builder);
        let query = Expression::new_string_literal(SPAN, "import", None, &builder);
        let mut args = oxc_allocator::Vec::new_in(&self.allocator);
        args.push(Argument::from(url));
        args.push(Argument::from(query));
        it.source = Expression::new_call_expression(span, callee, None, args, false, &builder);
        self.wrapped = true;
    }
}

#[cfg(test)]
mod tests {

    use std::path::Path;

    use crate::compile::{compile, compile_module, CompileOptions};

    #[test]
    fn rewrites_relative_specifiers_and_collects_imports() {
        let src = r#"
import { App } from "./App";
export { helper } from "../lib/helper";
import { useState } from "react";
export function Root() {
  const [n] = useState(0);
  return <App key={n} />;
}
"#;
        let mut rewrite = |spec: &str| -> Option<String> {
            spec.starts_with('.')
                .then(|| format!("/resolved{}", spec.trim_start_matches('.')))
        };
        let out = compile_module(
            Path::new("Root.tsx"),
            src,
            &CompileOptions::prod(),
            Some(&mut rewrite),
        )
        .unwrap();
        assert!(out.code.contains("\"/resolved/App\""), "{}", out.code);
        assert!(
            out.code.contains("\"/resolved/lib/helper\""),
            "{}",
            out.code
        );
        assert!(
            out.code.contains("\"react\""),
            "bare imports stay untouched"
        );
        assert!(out.imports.contains(&"/resolved/App".to_string()));
        assert!(out.imports.contains(&"react".to_string()));
        assert!(
            out.imports.iter().any(|i| i.contains("jsx-runtime")),
            "{:?}",
            out.imports
        );
    }

    #[test]
    fn erases_type_only_imports_from_code_and_collected_imports() {
        let src = r#"
import type { A } from "./types";
import { type B, c } from "./mixed";
import { d } from "./real";
export const used: A extends B ? number : number = c + d;
"#;
        let out = compile(Path::new("m.ts"), src, &CompileOptions::prod()).unwrap();
        assert!(
            !out.imports.iter().any(|i| i.contains("types")),
            "type-only import erased: {:?}",
            out.imports
        );
        assert!(
            !out.code.contains("./types"),
            "type-only source gone:\n{}",
            out.code
        );
        assert!(
            out.imports.iter().any(|i| i.contains("mixed")),
            "mixed import kept: {:?}",
            out.imports
        );
        assert!(
            !out.code.contains("type B"),
            "inline type specifier erased:\n{}",
            out.code
        );
        assert!(out.imports.iter().any(|i| i.contains("real")));
    }

    /// Vite's importAnalysis removes a static import's or re-export's
    /// attributes from the code it serves (`str().remove(end + 1, expEnd)`
    /// when es-module-lexer reports one); a dynamic import keeps its
    /// options. Without a rewriter (no dev serve) the code keeps them all.
    #[test]
    fn a_served_static_import_drops_its_attributes() {
        let src = r#"import data from "./data.json" with { type: "json" };
export { default as more } from "./more.json" with { type: "json" };
export * from "./all.json" with { type: "json" };
const later = import("./later.json", { with: { type: "json" } });
console.log(data, later);
"#;
        let mut rewrite = |spec: &str| -> Option<String> { Some(format!("/src/{}", &spec[2..])) };
        let out = compile_module(
            Path::new("main.js"),
            src,
            &CompileOptions::prod(),
            Some(&mut rewrite),
        )
        .unwrap();
        for spec in ["/src/data.json", "/src/more.json", "/src/all.json"] {
            let line = out
                .code
                .lines()
                .find(|l| l.contains(&format!("\"{spec}\"")) && !l.contains("import("))
                .unwrap_or_else(|| panic!("{spec}:\n{}", out.code));
            assert!(!line.contains("with"), "{line}");
        }
        assert!(
            out.code.contains("type: \"json\""),
            "the dynamic import keeps its options:\n{}",
            out.code
        );
        let kept =
            compile_module(Path::new("main.js"), src, &CompileOptions::prod(), None).unwrap();
        assert_eq!(
            kept.code.matches("with {").count() + kept.code.matches("with:").count(),
            4,
            "{}",
            kept.code
        );
    }

    /// The SSR compile serves a module runner that honours import
    /// attributes, where Vite's ssrTransform turns every static import into
    /// a `__vite_ssr_import__` call: with no rewriter (the SSR pipeline
    /// rewrites nothing) an `ssr` compile still drops them.
    #[test]
    fn an_ssr_compile_drops_static_import_attributes() {
        let src = r#"import recipe from "./recipe.json" with { type: "json" };
export { default as pantry } from "./pantry.json" with { type: "json" };
const later = import("./later.json", { with: { type: "json" } });
console.log(recipe, later);
"#;
        let mut opts = CompileOptions::dev();
        opts.refresh = false;
        opts.ssr = true;
        let out = compile_module(Path::new("recipe.js"), src, &opts, None).unwrap();
        assert!(
            !out.code.contains("with {") && out.code.contains("type: \"json\""),
            "the SSR compile keeps a static import's attributes:\n{}",
            out.code
        );
        assert_eq!(out.imports, vec!["./recipe.json", "./pantry.json"]);
    }

    #[test]
    fn rewrites_dynamic_import_specifiers() {
        let src = r#"export async function load() { return import("./chunk"); }"#;
        let mut rewrite = |s: &str| -> Option<String> {
            s.starts_with('.')
                .then(|| format!("/res{}", s.trim_start_matches('.')))
        };
        let out = compile_module(
            Path::new("d.ts"),
            src,
            &CompileOptions::prod(),
            Some(&mut rewrite),
        )
        .unwrap();
        assert!(
            out.code.contains("import(\"/res/chunk\")"),
            "dynamic import rewritten:\n{}",
            out.code
        );
        assert!(
            out.dynamic_imports.contains(&"/res/chunk".to_string()),
            "dynamic spec collected: {:?}",
            out.dynamic_imports
        );
        assert!(
            !out.imports.contains(&"/res/chunk".to_string()),
            "dynamic not in static imports"
        );
    }

    // A computed `import(x)` is wrapped unless its quoted text names a script
    // or a stylesheet (Vite's `urlIsStringRE` and `isExplicitImportRequired`).
    #[test]
    fn a_non_literal_dynamic_import_asks_for_the_module_on_the_dev_client() {
        let src = "const name = \"./data.json\";\nconst c = true;\nconst ext = \".js\";\n\
export const a = import(name);\n\
export const b = import(\"./c.js\");\n\
export const c2 = import(`./d.js`);\n\
export const d = import(`./${name}`);\n\
export const e = import(`/src/p/${name}.js`);\n\
export const f = import(`./${name}.json`);\n\
export const g = import('./p/' + name + '.js');\n\
export const h = import('./l/' + name + '.json');\n\
export const i = import(c ? './a.js' : './b.js');\n\
export const j = import('./a' + ext);\n\
export const k = import(`./m/${\nname}.js`);\n";
        let mut rewrite = |s: &str| -> Option<String> { Some(s.replace("./", "/src/")) };
        let opts = CompileOptions {
            inject_query_from: Some("/@oj/client.js".into()),
            ..CompileOptions::dev()
        };
        let out =
            compile_module(Path::new("/app/src/m.js"), src, &opts, Some(&mut rewrite)).unwrap();
        let code = &out.code;
        assert!(
            code.contains("import { injectQuery as __vite__injectQuery } from \"/@oj/client.js\";"),
            "the helper is imported from the client:\n{code}"
        );
        assert!(
            code.contains("import(\"/src/c.js\")") && code.contains("import(\"/src/d.js\")"),
            "literal imports are rewritten, not wrapped:\n{code}"
        );
        for kept in [
            "import(`./${name}`)",
            "import(`/src/p/${name}.js`)",
            "import(\"./p/\" + name + \".js\")",
        ] {
            assert!(code.contains(kept), "left as written: {kept}\n{code}");
        }
        for wrapped in [
            "import(__vite__injectQuery(name, \"import\"))",
            "import(__vite__injectQuery(`./${name}.json`, \"import\"))",
            "import(__vite__injectQuery(\"./l/\" + name + \".json\", \"import\"))",
            "import(__vite__injectQuery(c ? \"./a.js\" : \"./b.js\", \"import\"))",
            "import(__vite__injectQuery(\"./a\" + ext, \"import\"))",
        ] {
            assert!(code.contains(wrapped), "wrapped: {wrapped}\n{code}");
        }
        assert_eq!(
            code.matches("__vite__injectQuery(").count(),
            6,
            "the multi-line template is wrapped too (`.` matches no line break):\n{code}"
        );
        assert!(
            !out.imports.iter().any(|i| i.contains("client")),
            "the helper's import is a route, not a graph edge: {:?}",
            out.imports
        );
    }

    // Vite's isExplicitImportRequired, as its isJSRequest and CSS_LANGS_RE
    // read a url.
    #[test]
    fn explicit_import_is_required_for_neither_a_script_nor_a_stylesheet() {
        use super::explicit_import_required as req;
        for url in [
            "/a.json",
            "/a.svg?react",
            "/a.md?raw",
            "./${n}.json",
            "/dir/",
            "/a.JS",
        ] {
            assert!(req(url), "{url}");
        }
        for url in [
            "/a.js",
            "/a.tsx?t=1",
            "/a.vue",
            "/a",
            "./${n}",
            "/a.css",
            "/a.scss?inline",
        ] {
            assert!(!req(url), "{url}");
        }
    }

    // A compile without the option leaves `import(x)` as written.
    #[test]
    fn a_non_literal_dynamic_import_is_untouched_without_the_client() {
        let src = "const name = \"./data.json\";\nexport const a = import(name);\n";
        for opts in [
            CompileOptions::prod(),
            CompileOptions::dev(),
            CompileOptions {
                ssr: true,
                ..CompileOptions::dev()
            },
        ] {
            let plain = compile_module(Path::new("/app/src/m.js"), src, &opts, None).unwrap();
            assert!(
                plain.code.contains("import(name)") && !plain.code.contains("injectQuery"),
                "{}",
                plain.code
            );
        }
    }

    #[test]
    fn import_bindings_name_what_each_importer_uses() {
        let mut rw = |spec: &str| Some(spec.replace("./", "/src/"));
        let out = compile_module(
            Path::new("a.ts"),
            concat!(
                "import { a, b as c } from './mod.js';\n",
                "import def from './mod.js';\n",
                "import * as ns from './star.js';\n",
                "import './side.js';\n",
                "export { x } from './re.js';\n",
                "const d = await import('./dyn.js');\n",
                "console.log(a, c, def, ns, d);\n",
            ),
            &CompileOptions::dev(),
            Some(&mut rw),
        )
        .unwrap();
        let get = |spec: &str| {
            out.import_bindings
                .iter()
                .find(|(s, _)| s == spec)
                .map(|(_, names)| names.clone())
                .unwrap_or_else(|| panic!("{spec} missing from {:?}", out.import_bindings))
        };
        assert_eq!(get("/src/mod.js"), vec!["a", "b", "default"]);
        assert_eq!(get("/src/star.js"), vec!["*"]);
        assert_eq!(get("/src/side.js"), Vec::<String>::new());
        assert_eq!(get("/src/re.js"), vec!["x"]);
        assert_eq!(get("/src/dyn.js"), vec!["*"]);
    }
}
