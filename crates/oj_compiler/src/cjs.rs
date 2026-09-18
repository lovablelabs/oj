// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    AssignmentExpression, AssignmentOperator, AssignmentTarget, CallExpression, Expression,
    ObjectPropertyKind, PropertyKey, Statement,
};
use oxc_ast_visit::{walk, Visit};
use oxc_codegen::Codegen;
use oxc_minifier::{CompressOptions, Compressor};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer_plugins::{ReplaceGlobalDefines, ReplaceGlobalDefinesConfig};

use crate::{CompileError, CompileOutput};

pub fn compile_dep(
    path: &Path,
    url: &str,
    source_text: &str,
    resolve: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<CompileOutput, CompileError> {
    if has_module_syntax(path, source_text) {
        let opts = crate::CompileOptions {
            dev: true,
            refresh: false,
            sourcemap: false,
            ssr: false,
            jsx: crate::JsxConfig::default(),
        };
        crate::compile_module(path, source_text, &opts, Some(resolve))
    } else {
        wrap_cjs(path, url, source_text, resolve)
    }
}

pub fn has_module_syntax_pub(path: &Path, source_text: &str) -> bool {
    has_module_syntax(path, source_text)
}

#[derive(Debug)]
pub struct CjsFactoryAnalysis {
    pub body: String,
    pub requires: Vec<String>,
    /// Statically-detected `exports.X = ...` / `module.exports = { X }` names,
    /// used to re-export a bundled package entry's members as ESM named exports.
    pub named_exports: Vec<String>,
    /// `require()`d specifiers that are re-export sources (`module.exports =
    /// require("./x")` / `__exportStar`), so a bundler can pull their names too.
    pub reexport_requires: Vec<String>,
}

pub fn analyze_for_factory(
    path: &Path,
    source_text: &str,
) -> Result<CjsFactoryAnalysis, CompileError> {
    let (body, analysis) = lower_and_analyze(path, source_text)?;
    let named_exports = analysis.named_exports.clone();
    let reexport_requires = analysis.reexport_requires.clone();
    let mut requires = analysis.requires;
    requires.extend(analysis.reexport_requires);
    let mut seen = std::collections::HashSet::new();
    requires.retain(|s| seen.insert(s.clone()));
    Ok(CjsFactoryAnalysis { body, requires, named_exports, reexport_requires })
}

fn has_module_syntax(_path: &Path, source_text: &str) -> bool {
    let allocator = crate::pooled_allocator();
    let parsed = Parser::new(&allocator, source_text, SourceType::mjs()).parse();
    if parsed.panicked {
        return false;
    }
    parsed.program.body.iter().any(|stmt| {
        matches!(
            stmt,
            Statement::ImportDeclaration(_)
                | Statement::ExportDeclaration(_)
                | Statement::ExportNamedDeclaration(_)
                | Statement::ExportFromDeclaration(_)
                | Statement::ExportAllDeclaration(_)
                | Statement::ExportDefaultDeclaration(_)
        )
    })
}

fn lower_and_analyze(
    path: &Path,
    source_text: &str,
) -> Result<(String, CjsAnalyzer), CompileError> {
    let allocator = crate::pooled_allocator();
    let parsed = Parser::new(&allocator, source_text, SourceType::cjs()).parse();
    if parsed.panicked {
        let message = parsed
            .diagnostics
            .into_iter()
            .map(|d| format!("{d:?}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(CompileError::Parse {
            path: path.to_path_buf(),
            message,
        });
    }
    let mut program = parsed.program;

    let scoping = SemanticBuilder::new()
        .build(&program)
        .semantic
        .into_scoping();
    let config = ReplaceGlobalDefinesConfig::new(&[("process.env.NODE_ENV", "'development'")])
        .expect("static define config");
    let _ = ReplaceGlobalDefines::new(&allocator, config).build(scoping, &mut program);

    Compressor::new(&allocator).dead_code_elimination(&mut program, CompressOptions::dce());

    let mut analysis = CjsAnalyzer::default();
    analysis.visit_program(&program);

    Ok((Codegen::new().build(&program).code, analysis))
}

pub fn wrap_cjs(
    path: &Path,
    url: &str,
    source_text: &str,
    resolve: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<CompileOutput, CompileError> {
    let (body, analysis) = lower_and_analyze(path, source_text)?;

    let mut out = String::new();
    let mut deps = String::new();
    let mut resolved_imports: Vec<String> = Vec::new();
    let mut unresolved: Vec<&str> = Vec::new();

    let mut unique_requires = analysis.requires.clone();
    unique_requires.dedup();
    let unique_requires: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        unique_requires
            .into_iter()
            .filter(|s| seen.insert(s.clone()))
            .collect()
    };

    for (i, spec) in unique_requires.iter().enumerate() {
        match resolve(spec) {
            Some(dep_url) => {
                // Namespace import, not `{ __cjs_exports }`: a required dep may be
                // a genuine ESM module (e.g. an aliased polyfill) with no
                // __cjs_exports. __oj_cjs_interop unwraps oj-compiled CJS to its
                // module.exports and passes an ESM namespace through as-is.
                out.push_str(&format!("import * as __oj_ns_{i} from {dep_url:?};\n"));
                deps.push_str(&format!("  {spec:?}: __oj_cjs_interop(__oj_ns_{i}),\n"));
                resolved_imports.push(dep_url);
            }
            None => unresolved.push(spec),
        }
    }
    if analysis.has_dynamic_require {
        out.push_str(&format!(
            "console.warn(\"[oj] {url} contains dynamic require(); calls will throw\");\n"
        ));
    }

    out.push_str(&format!(
        r#"function __oj_cjs_interop(ns) {{
  return ns && Object.prototype.hasOwnProperty.call(ns, "__cjs_exports") ? ns.__cjs_exports : ns;
}}
const __oj_deps = {{
{deps}}};
const module = {{ exports: {{}} }};
var exports = module.exports;
function require(id) {{
  if (Object.prototype.hasOwnProperty.call(__oj_deps, id)) return __oj_deps[id];
  throw new Error("[oj] unresolved require(" + JSON.stringify(id) + ") in {url}");
}}
const __filename = {url:?};
const __dirname = {dirname:?};
(function () {{
{body}
}}).call(module.exports);
export const __cjs_exports = module.exports;
export default (module.exports && module.exports.__esModule) ? module.exports["default"] : module.exports;
"#,
        dirname = url.rsplit_once('/').map(|(d, _)| d).unwrap_or(""),
    ));

    for spec in &analysis.reexport_requires {
        if let Some(dep_url) = resolve(spec) {
            out.push_str(&format!("export * from {dep_url:?};\n"));
            if !resolved_imports.contains(&dep_url) {
                resolved_imports.push(dep_url);
            }
        }
    }

    let mut seen = std::collections::HashSet::new();
    for (i, name) in analysis
        .named_exports
        .iter()
        .filter(|n| is_valid_export_name(n) && seen.insert(n.as_str()))
        .enumerate()
    {
        out.push_str(&format!(
            "const __oj_export_{i} = module.exports[{name:?}];\nexport {{ __oj_export_{i} as {name} }};\n"
        ));
    }

    Ok(CompileOutput {
        code: out,
        map_data_url: None,
        imports: resolved_imports,
        dynamic_imports: Vec::new(),
        is_refresh_boundary: false,
        hot_accept: None,
    })
}

// The simple name a callee ultimately invokes, unwrapping member access and the
// `(0, obj.method)` sequence form emitted by transpilers.
fn call_name<'a>(expr: &'a Expression) -> Option<&'a str> {
    match expr {
        Expression::Identifier(id) => Some(id.name.as_str()),
        Expression::StaticMemberExpression(m) => Some(m.property.name.as_str()),
        Expression::ParenthesizedExpression(p) => call_name(&p.expression),
        Expression::SequenceExpression(s) => s.expressions.last().and_then(call_name),
        _ => None,
    }
}

fn is_export_star_helper(callee: &Expression) -> bool {
    matches!(
        call_name(callee),
        Some("__exportStar" | "__export" | "_export_star" | "__reExport")
    )
}

fn is_valid_export_name(name: &str) -> bool {
    if name == "default" || name == "__cjs_exports" || name.starts_with("__oj_") {
        return false;
    }
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

#[derive(Default)]
struct CjsAnalyzer {
    requires: Vec<String>,
    named_exports: Vec<String>,
    reexport_requires: Vec<String>,
    has_dynamic_require: bool,
}

fn require_specifier<'a>(call: &'a CallExpression) -> Option<&'a str> {
    let Expression::Identifier(callee) = &call.callee else {
        return None;
    };
    if callee.name != "require" || call.arguments.len() != 1 {
        return None;
    }
    match call.arguments[0].as_expression() {
        Some(Expression::StringLiteral(s)) => Some(s.value.as_str()),
        _ => None,
    }
}

fn is_exports_expression(expr: &Expression) -> bool {
    match expr {
        Expression::Identifier(id) => id.name == "exports",
        Expression::StaticMemberExpression(member) => {
            matches!(&member.object, Expression::Identifier(id) if id.name == "module")
                && member.property.name == "exports"
        }
        _ => false,
    }
}

impl<'a> Visit<'a> for CjsAnalyzer {
    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        if let Expression::Identifier(callee) = &it.callee {
            if callee.name == "require" {
                match require_specifier(it) {
                    Some(spec) => self.requires.push(spec.to_string()),
                    None => self.has_dynamic_require = true,
                }
            }
        }
        // TypeScript/tslib/swc compile `export * from "x"` to a helper call
        // (`__exportStar(require("x"), exports)`, `__export(require("x"))`,
        // `tslib_1.__exportStar(...)`, `(0, tslib_1.__exportStar)(...)`). Without
        // this a barrel like @sniptt/guards re-exporting its submodules exposes
        // no named exports, so `import { isUndefined }` fails.
        if is_export_star_helper(&it.callee) {
            if let Some(Expression::CallExpression(inner)) =
                it.arguments.first().and_then(|a| a.as_expression())
            {
                if let Some(spec) = require_specifier(inner) {
                    self.reexport_requires.push(spec.to_string());
                }
            }
        }
        walk::walk_call_expression(self, it);
    }

    fn visit_assignment_expression(&mut self, it: &AssignmentExpression<'a>) {
        if it.operator == AssignmentOperator::Assign {
            match &it.left {
                AssignmentTarget::StaticMemberExpression(member)
                    if is_exports_expression(&member.object) =>
                {
                    self.named_exports.push(member.property.name.to_string());
                }
                AssignmentTarget::StaticMemberExpression(member)
                    if member.property.name == "exports"
                        && matches!(&member.object, Expression::Identifier(id) if id.name == "module") =>
                {
                    self.collect_module_exports_value(&it.right);
                }
                _ => {}
            }
        }
        walk::walk_assignment_expression(self, it);
    }
}

impl CjsAnalyzer {
    fn collect_module_exports_value(&mut self, value: &Expression) {
        match value {
            Expression::CallExpression(call) => {
                if let Some(spec) = require_specifier(call) {
                    self.reexport_requires.push(spec.to_string());
                }
            }
            Expression::ObjectExpression(obj) => {
                for prop in &obj.properties {
                    if let ObjectPropertyKind::ObjectProperty(p) = prop {
                        match &p.key {
                            PropertyKey::StaticIdentifier(id) => {
                                self.named_exports.push(id.name.to_string());
                            }
                            PropertyKey::StringLiteral(s) => {
                                self.named_exports.push(s.value.to_string());
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn ts_compiled_export_star_helper_becomes_a_reexport() {
        // TypeScript compiles `export * from "./primitives"` to a standalone
        // `__exportStar(require("./primitives"), exports)` call, and tslib to
        // `(0, tslib_1.__exportStar)(require("./primitives"), exports)`. Both
        // must surface as `export * from` or a barrel like @sniptt/guards loses
        // its named exports.
        let src = r#"
"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
var tslib_1 = require("tslib");
__exportStar(require("./guards/primitives"), exports);
(0, tslib_1.__exportStar)(require("./guards/convenience"), exports);
"#;
        let mut resolve = |spec: &str| -> Option<String> {
            Some(format!("/node_modules/@sniptt/guards/build/{}.js", spec.trim_start_matches("./")))
        };
        let out = wrap_cjs(
            Path::new("index.js"),
            "/node_modules/@sniptt/guards/build/index.js",
            src,
            &mut resolve,
        )
        .unwrap();
        assert!(
            out.code.contains(r#"export * from "/node_modules/@sniptt/guards/build/guards/primitives.js""#),
            "plain __exportStar(require()) must re-export:\n{}",
            out.code
        );
        assert!(
            out.code.contains(r#"export * from "/node_modules/@sniptt/guards/build/guards/convenience.js""#),
            "tslib (0, tslib_1.__exportStar)(require()) must re-export:\n{}",
            out.code
        );
    }

    #[test]
    fn required_dep_is_namespace_imported_and_interopped() {
        // A required dep may be genuine ESM (e.g. an aliased polyfill) with no
        // __cjs_exports; the require must go through a namespace import and the
        // interop helper, never `import { __cjs_exports }` (which throws when
        // the dep is ESM).
        let src = r#"
"use strict";
var path = require("path");
module.exports = { join: path.join };
"#;
        let mut resolve =
            |_spec: &str| -> Option<String> { Some("/node_modules/rpnp/polyfills/path.js".to_string()) };
        let out = wrap_cjs(Path::new("m.js"), "/node_modules/pkg/m.js", src, &mut resolve).unwrap();
        assert!(
            out.code.contains(r#"import * as __oj_ns_0 from "/node_modules/rpnp/polyfills/path.js""#),
            "require target must be namespace-imported:\n{}",
            out.code
        );
        assert!(
            out.code.contains("function __oj_cjs_interop(ns)")
                && out.code.contains("__oj_cjs_interop(__oj_ns_0)"),
            "require value must pass through the interop helper:\n{}",
            out.code
        );
        assert!(
            !out.code.contains("{ __cjs_exports as"),
            "must not import __cjs_exports by name from a possibly-ESM dep:\n{}",
            out.code
        );
    }

    #[test]
    fn node_env_branch_becomes_single_star_reexport() {
        let src = r#"
'use strict';
if (process.env.NODE_ENV === 'production') {
  module.exports = require('./cjs/react.production.js');
} else {
  module.exports = require('./cjs/react.development.js');
}
"#;
        let mut resolve = |spec: &str| -> Option<String> {
            Some(format!(
                "/node_modules/react{}",
                spec.trim_start_matches('.')
            ))
        };
        let out = wrap_cjs(
            Path::new("index.js"),
            "/node_modules/react/index.js",
            src,
            &mut resolve,
        )
        .unwrap();
        assert!(
            !out.code.contains("production.js"),
            "production branch must be DCE'd:\n{}",
            out.code
        );
        assert!(out
            .code
            .contains(r#"export * from "/node_modules/react/cjs/react.development.js""#));
        assert!(out.imports.iter().all(|i| !i.contains("production")));
    }

    #[test]
    fn detects_named_exports_at_depth_and_wraps_body() {
        let src = r#"
'use strict';
(function () {
  function useState(x) { return [x, function () {}]; }
  exports.useState = useState;
  exports.version = "19.0.0";
  module.exports.Children = {};
})();
"#;
        let mut resolve = |_: &str| None;
        let out = wrap_cjs(Path::new("dev.js"), "/n/react/dev.js", src, &mut resolve).unwrap();
        for expected in [
            "export { __oj_export_0 as useState }",
            "as version }",
            "as Children }",
            "export const __cjs_exports = module.exports;",
            "export default",
        ] {
            assert!(
                out.code.contains(expected),
                "missing {expected:?}:\n{}",
                out.code
            );
        }
    }

    #[test]
    fn requires_become_static_imports_of_wrappers() {
        let src = r#"
var react = require('react');
var scheduler = require('scheduler');
exports.render = function () { return react && scheduler; };
"#;
        let mut resolve = |spec: &str| Some(format!("/node_modules/{spec}/index.js"));
        let out = wrap_cjs(Path::new("x.js"), "/n/x.js", src, &mut resolve).unwrap();
        assert!(out.code.contains(
            r#"import * as __oj_ns_0 from "/node_modules/react/index.js""#
        ));
        assert!(out.code.contains(r#""scheduler": __oj_cjs_interop(__oj_ns_1)"#));
        assert_eq!(out.imports.len(), 2);
    }

    #[test]
    fn fake_esm_default_honors_esmodule_flag() {
        let src = r#"
exports.__esModule = true;
exports.default = function Thing() {};
exports.named = 1;
"#;
        let mut resolve = |_: &str| None;
        let out = wrap_cjs(Path::new("f.js"), "/n/f.js", src, &mut resolve).unwrap();
        assert!(out
            .code
            .contains(r#"__esModule) ? module.exports["default"] : module.exports"#));
        assert!(out.code.contains("as named }"), "{}", out.code);
    }

    #[test]
    fn object_literal_module_exports_yield_named_exports() {
        let src = r#"module.exports = { alpha: 1, "beta": 2, [computed]: 3 };"#;
        let mut resolve = |_: &str| None;
        let out = wrap_cjs(Path::new("o.js"), "/n/o.js", src, &mut resolve).unwrap();
        assert!(out.code.contains("as alpha }"), "{}", out.code);
        assert!(out.code.contains("as beta }"), "{}", out.code);
        assert!(!out.code.contains("computed }"), "computed keys skipped");
    }

    #[test]
    fn dynamic_require_warns_loud_instead_of_breaking() {
        let src = r#"var x = require(someVar); exports.x = x;"#;
        let mut resolve = |_: &str| None;
        let out = wrap_cjs(Path::new("d.js"), "/n/d.js", src, &mut resolve).unwrap();
        assert!(out.code.contains("dynamic require"), "{}", out.code);
    }

    #[test]
    fn esm_deps_bypass_the_cjs_wrapper() {
        let src = r#"export const x = 1;"#;
        let mut resolve = |_: &str| None;
        let out = compile_dep(Path::new("m.js"), "/n/m.js", src, &mut resolve).unwrap();
        assert!(out.code.contains("export const x = 1"));
        assert!(!out.code.contains("__cjs_exports"));
    }

    #[test]
    fn has_module_syntax_detects_only_statement_import_export() {
        let p = Path::new("x.js");
        assert!(has_module_syntax_pub(p, "import x from 'y';"));
        assert!(has_module_syntax_pub(p, "export const a = 1;"));
        assert!(has_module_syntax_pub(p, "export { a } from './m';"));
        assert!(has_module_syntax_pub(p, "export * from './m';"));
        assert!(has_module_syntax_pub(p, "export default 1;"));
        assert!(!has_module_syntax_pub(p, "module.exports = { a: 1 };"));
        assert!(!has_module_syntax_pub(p, "const x = require('y');"));
        assert!(!has_module_syntax_pub(p, "const p = import('y');"));
    }

    #[test]
    fn is_valid_export_name_filters_reserved_and_internal() {
        for ok in ["foo", "_bar", "$x", "a1_$", "React"] {
            assert!(is_valid_export_name(ok), "{ok} should be valid");
        }
        for bad in [
            "default",
            "__cjs_exports",
            "__oj_glob_0",
            "1foo",
            "foo-bar",
            "",
            "a.b",
        ] {
            assert!(!is_valid_export_name(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn unresolved_require_is_omitted_and_guarded_at_runtime() {
        let src =
            r#"var missing = require('./gone'); exports.use = function () { return missing; };"#;
        let mut resolve = |_: &str| None;
        let out = wrap_cjs(Path::new("u.js"), "/n/u.js", src, &mut resolve).unwrap();
        assert!(
            !out.code.contains("__oj_dep_0"),
            "unresolved dep must not import: {}",
            out.code
        );
        assert!(
            out.imports.is_empty(),
            "unresolved dep is not an edge: {:?}",
            out.imports
        );
        assert!(
            out.code.contains("unresolved require("),
            "runtime guard present: {}",
            out.code
        );
    }

    #[test]
    fn duplicate_requires_dedupe_to_one_import() {
        let src = r#"var a = require('dep'); var b = require('dep'); exports.x = function () { return a || b; };"#;
        let mut resolve = |spec: &str| Some(format!("/node_modules/{spec}/index.js"));
        let out = wrap_cjs(Path::new("d.js"), "/n/d.js", src, &mut resolve).unwrap();
        assert_eq!(out.imports, vec!["/node_modules/dep/index.js".to_string()]);
        assert!(out.code.contains("__oj_ns_0"));
        assert!(
            !out.code.contains("__oj_ns_1"),
            "second require must dedupe: {}",
            out.code
        );
    }

    #[test]
    fn invalid_identifier_export_keys_are_dropped() {
        let src = r#"module.exports = { good: 1, "bad-name": 2, "with space": 3 };"#;
        let mut resolve = |_: &str| None;
        let out = wrap_cjs(Path::new("k.js"), "/n/k.js", src, &mut resolve).unwrap();
        assert!(
            out.code.contains("as good }"),
            "valid key exported: {}",
            out.code
        );
        assert!(
            !out.code.contains("as bad-name"),
            "hyphenated key not exported: {}",
            out.code
        );
        assert!(
            !out.code.contains("as with space"),
            "spaced key not exported: {}",
            out.code
        );
        assert!(
            !out.code.contains("__oj_export_1"),
            "only the valid key exported: {}",
            out.code
        );
        assert!(out.code.contains("export default"), "{}", out.code);
    }

    #[test]
    fn wrapper_injects_filename_and_dirname_from_url() {
        let src = r#"exports.here = __dirname; exports.file = __filename;"#;
        let mut resolve = |_: &str| None;
        let out = wrap_cjs(
            Path::new("x.js"),
            "/node_modules/pkg/sub/x.js",
            src,
            &mut resolve,
        )
        .unwrap();
        assert!(
            out.code
                .contains(r#"const __filename = "/node_modules/pkg/sub/x.js""#),
            "{}",
            out.code
        );
        assert!(
            out.code
                .contains(r#"const __dirname = "/node_modules/pkg/sub""#),
            "{}",
            out.code
        );
    }
}
