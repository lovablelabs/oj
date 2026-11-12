// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! TanStack Start's `createServerFn` rewrites, one implementation for every
//! side so they agree on which calls are server functions and on their ids:
//! the server provider (the dev SSR module host and the server bundle) and the
//! client RPC stub (the client bundles).
//!
//! Which calls count is decided on the module's AST, as Start's compiler
//! decides it (`@tanstack/start-plugin-core`, start-compiler/compiler.js): a
//! module variable, exported or not, `const`, `let` or `var`, with or without
//! a type annotation, whose initializer is a method chain ending in
//! `.handler(..)` and rooted at the factory. The factory is `createServerFn`
//! as imported, under its own name or another (`import { createServerFn as
//! fn }`), or as a namespace import's member (`start.createServerFn()`), and
//! a chain may run through another module variable (`const factory =
//! createServerFn(); export const x = factory.handler(..)`), as the compiler's
//! `resolveExprKind` follows a binding to the call that made it a Builder.
//! The variable may come from another of the app's modules (`import { base }
//! from "./base"`, `export const base = createServerFn()` there), reached
//! through its re-exports, as the compiler's `resolveImportKind` follows the
//! import to the binding that made it a Builder; so may the factory itself
//! (`export { createServerFn } from "@tanstack/react-start"`). The import
//! may be that module's default export (`import base from "./base"`, `export
//! default createServerFn()` there), which the compiler's `findExportInModule`
//! reads as any other name, and its specifier an alias the app's resolver
//! knows (a tsconfig `paths` entry, a package.json `imports` entry), which
//! the compiler resolves with Vite's `this.resolve` (see [`FsModules`]).
//! TypeScript's
//! `as`, `satisfies` and `!` around the chain are looked through
//! (`unwrapExpression`). A `.handler(..)` call that is not a variable's
//! initializer (a property of an object literal, a `return`) is not a server
//! function, as the compiler's `getVariableDeclaratorForExpressionPath` finds
//! none for it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, BindingPattern, CallExpression, Declaration, Expression, ImportDeclarationSpecifier,
    ModuleExportName, Program, Statement, VariableDeclaration,
};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

/// The factory's name as Start exports it.
const FACTORY: &str = "createServerFn";

/// The chain links the client drops (`stripMethodCall` in
/// handleCreateServerFn): the input validation runs on the server only.
const STRIPPED_LINKS: [&str; 2] = ["validator", "inputValidator"];

/// A `NAME = createServerFn(...)...handler(` module variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFn {
    pub name: String,
    /// Byte offset of the declaration statement (`export const`, `let`, ...).
    pub decl_start: usize,
    /// Byte offset just past the handler call's `(`.
    pub args_at: usize,
    /// The handler's first argument, as a span, when it has one.
    first_arg: Option<(usize, usize)>,
    /// `.validator(...)` / `.inputValidator(...)` links to delete on the
    /// client, as the span from the link's object end to the link's end.
    stripped_links: Vec<(usize, usize)>,
}

/// The id a server function is registered and called under:
/// `base64url("<app-relative path>#<name>")`, Node's `Buffer` encoding.
pub fn server_fn_id(rel: &str, name: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{rel}#{name}").as_bytes())
}

/// The id Start's own compiler gives the same function in Vite dev
/// (`generateFunctionId` in dev mode): `base64url(JSON.stringify({ file, export }))`,
/// `file` being the dev-server specifier of the split provider module
/// (`/<path>?tss-serverfn-split`) and `export` its handler. A module served
/// file by file, outside oj's bundles, is compiled by Start's plugin and calls
/// under this id.
pub fn vite_dev_server_fn_id(rel: &str, name: &str) -> String {
    use base64::Engine;
    let file = serde_json::Value::String(format!("/{rel}?tss-serverfn-split"));
    let export = serde_json::Value::String(format!("{name}_createServerFn_handler"));
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(format!("{{\"file\":{file},\"export\":{export}}}").as_bytes())
}

/// Whether `code` may declare a server function, as Start's compiler decides
/// which modules to read (`KindDetectionPatterns.ServerFn`): it names the
/// factory, or calls `.handler(..)`, whose chain may root at a builder another
/// module exports.
pub fn may_declare(code: &str) -> bool {
    static HANDLER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    code.contains(FACTORY)
        || HANDLER
            .get_or_init(|| regex::Regex::new(r"\.\s*handler\s*\(").expect("handler call regex"))
            .is_match(code)
}

/// The app's modules a chain may import its builder or factory from.
pub trait Modules {
    /// The module `specifier` names from the module at `from`, as its own
    /// app-relative path and its source. None for a specifier that is not one
    /// of the app's files (a package, a missing file): such a binding is
    /// nothing to the chain, as the compiler's unresolved import is.
    fn read(&self, from: &str, specifier: &str) -> Option<(String, String)>;
}

/// No other module: a chain roots in its own module only.
impl Modules for () {
    fn read(&self, _from: &str, _specifier: &str) -> Option<(String, String)> {
        None
    }
}

/// The app's files under `root`, a relative specifier resolved as Vite's
/// `tryCleanFsResolve` resolves it: the file as written; a `.js`, `.jsx`,
/// `.mjs` or `.cjs` import's TypeScript source; the default extensions in
/// Vite's order; then a directory's index. Any other specifier is resolved
/// by the app's resolver, as Start's compiler resolves an import with Vite's
/// `this.resolve` (start-compiler-plugin, the `resolveId` it hands
/// `StartCompiler`): a tsconfig `paths` alias (`@app/lib/base`) or a
/// package.json `imports` entry (`#lib/base`) names the app's module as a
/// relative specifier does, so a builder imported through one is followed on
/// both sides and the client calls the id the server registered. A file
/// outside the root or under `node_modules` is not the app's.
pub struct FsModules<'a> {
    root: &'a Path,
    resolver: std::sync::OnceLock<oj_resolver::OjResolver>,
}

impl<'a> FsModules<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self {
            root,
            resolver: std::sync::OnceLock::new(),
        }
    }

    /// `specifier` (not relative) as the app's module it names, or None.
    fn resolve_aliased(&self, from: &str, specifier: &str) -> Option<String> {
        // oxc's resolver, with the tsconfig that owns the importer applying
        // its `paths` as Vite 8's own resolver does (`resolve.tsconfigPaths`,
        // or vite-tsconfig-paths ahead of it). An app without either cannot
        // import through `paths` at all, so answering for it changes nothing.
        let resolver = self.resolver.get_or_init(|| {
            oj_resolver::OjResolver::with_settings(
                self.root,
                oj_resolver::ResolveSettings {
                    conditions: ["browser", "import", "module", "development", "default"]
                        .map(String::from)
                        .to_vec(),
                    tsconfig_paths: oj_resolver::TsconfigPaths::Native,
                    ..Default::default()
                },
            )
        });
        let resolved = resolver
            .resolve_from(&self.root.join(from), specifier)
            .ok()?;
        // The resolver answers with the real path; the root may be given
        // through a symlink.
        let rel = match resolved.strip_prefix(self.root) {
            Ok(rel) => slash(rel),
            Err(_) => slash(resolved.strip_prefix(self.root.canonicalize().ok()?).ok()?),
        };
        if rel.split('/').any(|part| part == "node_modules") {
            return None;
        }
        let ext = Path::new(&rel).extension().and_then(|e| e.to_str())?;
        EXTENSIONS.contains(&ext).then_some(rel)
    }
}

/// Vite's `DEFAULT_EXTENSIONS`, less `.json`, which declares nothing.
const EXTENSIONS: [&str; 6] = ["mjs", "js", "mts", "ts", "jsx", "tsx"];

impl Modules for FsModules<'_> {
    fn read(&self, from: &str, specifier: &str) -> Option<(String, String)> {
        let rel = if specifier.starts_with('.') {
            let dir = Path::new(from).parent().unwrap_or_else(|| Path::new(""));
            resolve_file(self.root, &normalize(&dir.join(specifier)))?
        } else {
            self.resolve_aliased(from, specifier)?
        };
        let code = std::fs::read_to_string(self.root.join(&rel)).ok()?;
        Some((rel, code))
    }
}

/// `rel` (app-relative, normalized) as the file Vite would load, app-relative
/// with `/` separators, or None when no file answers.
fn resolve_file(root: &Path, rel: &Path) -> Option<String> {
    let file = |candidate: &Path| root.join(candidate).is_file().then(|| slash(candidate));
    if let Some(found) = file(rel) {
        return Some(found);
    }
    // `isPossibleTsOutput`: a JS import may name its TypeScript source.
    if let Some(ext) = rel.extension().and_then(|e| e.to_str()) {
        if matches!(ext, "js" | "jsx" | "mjs" | "cjs") {
            let mut candidates = vec![rel.with_extension(ext.replace("js", "ts"))];
            if ext == "js" {
                candidates.push(rel.with_extension("tsx"));
            }
            if let Some(found) = candidates.iter().find_map(|c| file(c)) {
                return Some(found);
            }
        }
    }
    let as_str = slash(rel);
    if let Some(found) = EXTENSIONS
        .iter()
        .find_map(|ext| file(Path::new(&format!("{as_str}.{ext}"))))
    {
        return Some(found);
    }
    if root.join(rel).is_dir() {
        return EXTENSIONS
            .iter()
            .find_map(|ext| file(&rel.join(format!("index.{ext}"))));
    }
    None
}

/// `path` without `.` and `..` segments.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    out
}

fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Every server function declared in `code`, in source order. `path`, the
/// module's app-relative path, picks the parser's language and locates the
/// modules a chain imports from, read through `modules`. A module the parser
/// cannot read declares none.
pub fn find_server_fns(code: &str, path: &str, modules: &dyn Modules) -> Vec<ServerFn> {
    if !may_declare(code) {
        return Vec::new();
    }
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, source_type(path)).parse();
    if parsed.fatal_error {
        return Vec::new();
    }
    find_in(&parsed.program, code, path, modules)
}

fn source_type(path: &str) -> SourceType {
    SourceType::from_path(path).unwrap_or_else(|_| SourceType::tsx())
}

/// The server functions of a parsed module.
fn find_in(program: &Program, code: &str, path: &str, modules: &dyn Modules) -> Vec<ServerFn> {
    let bindings = Bindings::of(program, path, modules, &[]);
    let mut out = Vec::new();
    for (decl_start, var) in module_declarations(program) {
        for d in &var.declarations {
            let (BindingPattern::BindingIdentifier(id), Some(init)) = (&d.id, &d.init) else {
                continue;
            };
            let Expression::CallExpression(call) = init.get_inner_expression() else {
                continue;
            };
            let Some(("handler", property_end, chain)) = bindings.chain(call) else {
                continue;
            };
            let Some(args_at) = args_start(code, call, property_end) else {
                continue;
            };
            let first_arg = call.arguments.first().map(|a: &Argument| {
                let span = a.span();
                (span.start as usize, span.end as usize)
            });
            out.push(ServerFn {
                name: id.name.to_string(),
                decl_start,
                args_at,
                first_arg,
                stripped_links: chain,
            });
        }
    }
    out
}

/// The module's variable declarations, exported or not, each with the
/// offset of its statement (the `export` when there is one).
fn module_declarations<'a>(
    program: &'a Program<'a>,
) -> impl Iterator<Item = (usize, &'a VariableDeclaration<'a>)> {
    program.body.iter().filter_map(|statement| match statement {
        Statement::VariableDeclaration(var) => Some((var.span.start as usize, &**var)),
        Statement::ExportDeclaration(export) => match &export.declaration {
            Declaration::VariableDeclaration(var) => Some((export.span.start as usize, &**var)),
            _ => None,
        },
        _ => None,
    })
}

/// The offset just past the `(` of `call`, whose callee's property ends at
/// `property_end`: after any type arguments, whitespace and comments.
fn args_start(code: &str, call: &CallExpression, property_end: usize) -> Option<usize> {
    let from = call
        .type_arguments
        .as_ref()
        .map_or(property_end, |t| t.span.end as usize);
    let at = skip_ws_and_comments(code.as_bytes(), from)?;
    (code.as_bytes()[at] == b'(').then_some(at + 1)
}

/// The last link of a chain on the factory: its name, where the name ends,
/// and the `.validator(..)` / `.inputValidator(..)` links before it, as spans
/// from each link's object end to its end.
type LastLink<'s> = (&'s str, usize, Vec<(usize, usize)>);

/// What a name stands for in a chain: Start's `Root` (the factory) and
/// `Builder` (a chain on it short of `.handler(..)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Factory,
    Builder,
    None,
}

/// The module's names for the factory and for the chains built on it.
struct Bindings<'m> {
    modules: &'m dyn Modules,
    /// The module's own app-relative path, which its imports resolve from.
    path: String,
    /// This module and the modules whose exports are being resolved above it,
    /// so a cycle of re-exports ends.
    visiting: Vec<String>,
    /// Local names bound to `createServerFn`: the name itself, what an
    /// import of it is called locally, and a local that another of the app's
    /// modules exports it as.
    factories: Vec<String>,
    /// Namespace imports of packages, whose `createServerFn` member is the
    /// factory.
    namespaces: Vec<String>,
    /// Namespace imports of the app's own modules, with the specifier: a
    /// member may be a builder that module exports.
    module_namespaces: Vec<(String, String)>,
    /// Names holding a chain on the factory that has not reached
    /// `.handler(..)` yet (Start's `Builder`), which another chain may
    /// continue: module variables, and imports of such a variable from
    /// another of the app's modules.
    builders: Vec<String>,
    /// What another module exports under a name, once asked: by specifier
    /// and exported name.
    exported: RefCell<HashMap<(String, String), Kind>>,
}

impl<'m> Bindings<'m> {
    fn of(program: &Program, path: &str, modules: &'m dyn Modules, above: &[String]) -> Self {
        let mut visiting = above.to_vec();
        visiting.push(path.to_string());
        let mut out = Bindings {
            modules,
            path: path.to_string(),
            visiting,
            factories: vec![FACTORY.to_string()],
            namespaces: Vec::new(),
            module_namespaces: Vec::new(),
            builders: Vec::new(),
            exported: RefCell::new(HashMap::new()),
        };
        for statement in &program.body {
            let Statement::ImportDeclaration(import) = statement else {
                continue;
            };
            let source = crate::lit_str(import.source.value);
            // An import of the app's module (relative, or an alias the
            // resolver knows) is what that module exports under the name;
            // a package's is the factory when it is called so. The default
            // import is the `default` export, as the compiler's
            // `findExportInModule` reads it.
            for specifier in import.specifiers.iter().flatten() {
                match specifier {
                    ImportDeclarationSpecifier::ImportSpecifier(s) => {
                        let kind = out.exported_kind(source, export_name(&s.imported));
                        out.bind(s.local.name.as_str(), kind);
                    }
                    ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                        let kind = out.exported_kind(source, "default");
                        out.bind(s.local.name.as_str(), kind);
                    }
                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                        if out.is_own_module(source) {
                            out.module_namespaces
                                .push((s.local.name.to_string(), source.to_string()));
                        } else {
                            out.namespaces.push(s.local.name.to_string());
                        }
                    }
                }
            }
        }
        // A variable's chain may root at a variable declared after it: to a
        // fixed point, as the compiler resolves a binding wherever it is.
        let declarators: Vec<_> = module_declarations(program)
            .flat_map(|(_, var)| var.declarations.iter())
            .filter_map(|d| match (&d.id, &d.init) {
                (BindingPattern::BindingIdentifier(id), Some(init)) => {
                    Some((id.name.as_str(), init))
                }
                _ => None,
            })
            .collect();
        loop {
            let found: Vec<String> = declarators
                .iter()
                .filter(|(name, init)| {
                    !out.builders.iter().any(|b| b == name)
                        && out.kind_of_init(init) == Kind::Builder
                })
                .map(|(name, _)| name.to_string())
                .collect();
            if found.is_empty() {
                break;
            }
            out.builders.extend(found);
        }
        out
    }

    /// What a module variable's initializer makes it: the factory under
    /// another name, a builder, or neither.
    fn kind_of_init(&self, init: &Expression) -> Kind {
        let init = init.get_inner_expression();
        if self.is_factory(init) {
            return Kind::Factory;
        }
        match init {
            Expression::Identifier(id) => self.kind_of_name(id.name.as_str()),
            Expression::CallExpression(call) => {
                if self.is_factory(&call.callee)
                    || matches!(self.chain(call), Some((link, _, _)) if link != "handler")
                {
                    Kind::Builder
                } else {
                    Kind::None
                }
            }
            _ => Kind::None,
        }
    }

    /// What a local name stands for.
    fn kind_of_name(&self, name: &str) -> Kind {
        if self.factories.iter().any(|f| f == name) {
            Kind::Factory
        } else if self.builders.iter().any(|b| b == name) {
            Kind::Builder
        } else {
            Kind::None
        }
    }

    /// Binds a local name to what its import stands for.
    fn bind(&mut self, local: &str, kind: Kind) {
        match kind {
            Kind::Factory if local != FACTORY => self.factories.push(local.to_string()),
            Kind::Builder => self.builders.push(local.to_string()),
            Kind::Factory | Kind::None => {}
        }
    }

    /// Whether `specifier` names one of the app's modules: a relative
    /// path, or an alias `modules` resolves to one.
    fn is_own_module(&self, specifier: &str) -> bool {
        specifier.starts_with('.') || self.modules.read(&self.path, specifier).is_some()
    }

    /// What the module `specifier` names from this one exports as `name`,
    /// through `export const`, `export { local as name }`, `export { .. }
    /// from`, `export * from` and, for `default`, `export default`, as the
    /// compiler's `findExportInModule` follows them. None for a module that
    /// is not the app's, that the parser cannot read, that exports no such
    /// name, or that is already being resolved above this one.
    fn exported_kind(&self, specifier: &str, name: &str) -> Kind {
        let key = (specifier.to_string(), name.to_string());
        if let Some(kind) = self.exported.borrow().get(&key) {
            return *kind;
        }
        let kind = self.resolve_exported(specifier, name);
        self.exported.borrow_mut().insert(key, kind);
        kind
    }

    fn resolve_exported(&self, specifier: &str, name: &str) -> Kind {
        let Some((path, code)) = self.modules.read(&self.path, specifier) else {
            // A package's `createServerFn`, re-exported by the app's module,
            // is the factory, as a direct import of it is.
            return if !specifier.starts_with('.') && name == FACTORY {
                Kind::Factory
            } else {
                Kind::None
            };
        };
        if self.visiting.contains(&path) {
            return Kind::None;
        }
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, &code, source_type(&path)).parse();
        if parsed.fatal_error {
            return Kind::None;
        }
        let bindings = Bindings::of(&parsed.program, &path, self.modules, &self.visiting);
        for statement in &parsed.program.body {
            match statement {
                Statement::ExportDeclaration(export) => {
                    if let Declaration::VariableDeclaration(var) = &export.declaration {
                        for d in &var.declarations {
                            if let (BindingPattern::BindingIdentifier(id), Some(init)) =
                                (&d.id, &d.init)
                            {
                                if id.name == name {
                                    return bindings.kind_of_init(init);
                                }
                            }
                        }
                    }
                }
                Statement::ExportNamedDeclaration(export) => {
                    if let Some(s) = export
                        .specifiers
                        .iter()
                        .find(|s| export_name(&s.exported) == name)
                    {
                        return bindings.kind_of_name(export_name(&s.local));
                    }
                }
                Statement::ExportFromDeclaration(export) => {
                    if let Some(s) = export
                        .specifiers
                        .iter()
                        .find(|s| export_name(&s.exported) == name)
                    {
                        return bindings.exported_kind(
                            crate::lit_str(export.source.value),
                            export_name(&s.local),
                        );
                    }
                }
                Statement::ExportAllDeclaration(export) if export.exported.is_none() => {
                    let kind = bindings.exported_kind(crate::lit_str(export.source.value), name);
                    if kind != Kind::None {
                        return kind;
                    }
                }
                // `export default base` or `export default createServerFn()`:
                // the compiler's `extractModuleInfoFromAst` binds the
                // expression under `default`. A function or class is neither.
                Statement::ExportDefaultDeclaration(export) if name == "default" => {
                    return match export.declaration.as_expression() {
                        Some(expr) => bindings.kind_of_init(expr),
                        None => Kind::None,
                    };
                }
                _ => {}
            }
        }
        Kind::None
    }

    /// Whether `callee` is the factory: a name bound to it, or a namespace
    /// import's `createServerFn`.
    fn is_factory(&self, callee: &Expression) -> bool {
        match callee.without_parentheses() {
            Expression::Identifier(id) => self.factories.iter().any(|f| f == id.name.as_str()),
            Expression::StaticMemberExpression(m) => {
                m.property.name == FACTORY
                    && matches!(m.object.without_parentheses(), Expression::Identifier(object)
                        if self.namespaces.iter().any(|n| n == object.name.as_str()))
            }
            _ => false,
        }
    }

    /// Whether `node` names a builder: a local bound to one, or a member of
    /// a namespace import of the app's module that exports one.
    fn is_builder(&self, node: &Expression) -> bool {
        match node {
            Expression::Identifier(id) => self.builders.iter().any(|b| b == id.name.as_str()),
            Expression::StaticMemberExpression(m) => {
                let Expression::Identifier(object) = m.object.without_parentheses() else {
                    return false;
                };
                self.module_namespaces
                    .iter()
                    .find(|(local, _)| local == object.name.as_str())
                    .is_some_and(|(_, specifier)| {
                        self.exported_kind(specifier, m.property.name.as_str()) == Kind::Builder
                    })
            }
            _ => false,
        }
    }

    /// `call` as the last link of a chain rooted at the factory, or at a
    /// name holding a chain on it. None for a call that is the factory's
    /// own, or no chain on it.
    fn chain<'b>(&self, call: &'b CallExpression) -> Option<LastLink<'b>> {
        let Expression::StaticMemberExpression(m) = call.callee.without_parentheses() else {
            return None;
        };
        let name = m.property.name.as_str();
        let property_end = m.property.span.end as usize;
        let mut stripped = Vec::new();
        let mut node = m.object.without_parentheses();
        loop {
            match node {
                Expression::CallExpression(link) => {
                    if self.is_factory(&link.callee) {
                        break;
                    }
                    let Expression::StaticMemberExpression(inner) =
                        link.callee.without_parentheses()
                    else {
                        return None;
                    };
                    if STRIPPED_LINKS.contains(&inner.property.name.as_str()) {
                        stripped.push((inner.object.span().end as usize, link.span.end as usize));
                    }
                    node = inner.object.without_parentheses();
                }
                _ if self.is_builder(node) => break,
                _ => return None,
            }
        }
        stripped.reverse();
        Some((name, property_end, stripped))
    }
}

fn export_name<'a>(name: &'a ModuleExportName) -> &'a str {
    match name {
        ModuleExportName::IdentifierName(id) => id.name.as_str(),
        ModuleExportName::IdentifierReference(id) => id.name.as_str(),
        ModuleExportName::StringLiteral(lit) => crate::lit_str(lit.value),
    }
}

/// The server provider: every server function grows a sibling
/// `NAME_createServerFn_handler = createServerRpc(meta, ...)` export, passed as
/// the handler's first argument. Adds no lines, so stack traces keep their
/// positions. `rel` is the app-relative path, `modules` reads the app's
/// other modules for an imported builder. `None` when the module declares no
/// server function.
pub fn rewrite_server(code: &str, rel: &str, modules: &dyn Modules) -> Option<String> {
    let mut edits: Vec<(usize, String)> = Vec::new();
    for ServerFn {
        name,
        decl_start,
        args_at,
        ..
    } in find_server_fns(code, rel, modules)
    {
        let id = server_fn_id(rel, &name);
        let meta = format!("{{ id: {id:?}, name: {name:?}, filename: {rel:?} }}");
        edits.push((
            decl_start,
            format!(
                "export const {name}_createServerFn_handler = createServerRpc({meta}, (opts) => {name}.__executeServer(opts)); "
            ),
        ));
        edits.push((args_at, format!("{name}_createServerFn_handler, ")));
    }
    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|(at, _)| *at);
    let mut out =
        String::from("import { createServerRpc } from \"@tanstack/react-start/server-rpc\"; ");
    let mut last = 0;
    for (at, text) in edits {
        out.push_str(&code[last..at]);
        out.push_str(&text);
        last = at;
    }
    out.push_str(&code[last..]);
    Some(out)
}

/// A client rewrite that could not be applied: a server function whose
/// handler call has no argument to replace would ship its handler to the
/// browser, so this is fatal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientRewriteError(pub String);

impl std::fmt::Display for ClientRewriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ClientRewriteError {}

/// The client caller, edited on the module's AST the way TanStack's
/// `handleCreateServerFn` edits its Babel AST: the handler's first argument
/// becomes `createClientRpc(id)` and every `.validator()` / `.inputValidator()`
/// link is dropped, so neither the handler nor the validator reaches the
/// browser. A module-level binding only the dropped code used goes too, as
/// Start's dead-code pass drops it. Line counts are kept. `rel` is the
/// app-relative path, which also picks the parser's language; `modules` reads
/// the app's other modules for an imported builder. `Ok(None)` when the module
/// declares no server function. A module that names the factory but does not
/// parse is an error: it may hold a handler that would ship.
pub fn rewrite_client(
    code: &str,
    rel: &str,
    modules: &dyn Modules,
) -> Result<Option<String>, ClientRewriteError> {
    if !may_declare(code) {
        return Ok(None);
    }
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, source_type(rel)).parse();
    if parsed.fatal_error {
        if !code.contains(FACTORY) {
            return Ok(None);
        }
        return Err(ClientRewriteError(format!(
            "oj: could not parse {rel} to rewrite its server functions for the client"
        )));
    }
    let fns = find_in(&parsed.program, code, rel, modules);
    if fns.is_empty() {
        return Ok(None);
    }

    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for f in &fns {
        let Some((arg_start, arg_end)) = f.first_arg else {
            return Err(ClientRewriteError(format!(
                "oj: could not rewrite server function {} in {rel} for the client",
                f.name
            )));
        };
        edits.push((
            arg_start,
            arg_end,
            format!("createClientRpc({:?})", server_fn_id(rel, &f.name)),
        ));
        for &(start, end) in &f.stripped_links {
            edits.push((start, end, String::new()));
        }
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut out = code.to_string();
    let mut step = Vec::with_capacity(edits.len());
    for (start, end, text) in edits {
        let lines = code[start..end].matches('\n').count();
        step.push(crate::dce::Replaced {
            start,
            end,
            len: text.len() + lines,
            moved: Vec::new(),
        });
        out.replace_range(start..end, &format!("{text}{}", "\n".repeat(lines)));
    }
    let mut map = crate::dce::EditMap::default();
    map.push(step);
    // Start's compiler then runs its dead-code pass over the module: what only
    // the handler or a validator used (an import of
    // `@tanstack/react-start/server`, a helper) leaves the client with them.
    let out = crate::env_fns::drop_dead_bindings(code, out, rel, &map);
    // After the directive prologue (or a hashbang), on its line: the rewrite
    // must not shift source lines, nor turn `"use client"` into a statement.
    let at = prologue_end(&parsed.program, code);
    let sep = if at > 0 && !code[..at].ends_with('\n') {
        " "
    } else {
        ""
    };
    Ok(Some(format!(
        "{}{sep}import {{ createClientRpc }} from \"@tanstack/react-start/client-rpc\"; {}",
        &out[..at],
        &out[at..]
    )))
}

fn prologue_end(program: &Program, code: &str) -> usize {
    if let Some(last) = program.directives.last() {
        return last.span.end as usize;
    }
    if code.starts_with("#!") {
        return code.find('\n').map_or(code.len(), |i| i + 1);
    }
    0
}

fn skip_ws_and_comments(bytes: &[u8], mut i: usize) -> Option<usize> {
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 >= bytes.len() {
                return None;
            }
            i += 2;
            continue;
        }
        return if i < bytes.len() { Some(i) } else { None };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The module's own chains: no other module to import a builder from.
    fn find_server_fns(code: &str, path: &str) -> Vec<ServerFn> {
        super::find_server_fns(code, path, &())
    }

    fn rewrite_client(code: &str, rel: &str) -> Result<Option<String>, ClientRewriteError> {
        super::rewrite_client(code, rel, &())
    }

    fn rewrite_server(code: &str, rel: &str) -> Option<String> {
        super::rewrite_server(code, rel, &())
    }

    const CLIENT_IMPORT: &str =
        "import { createClientRpc } from \"@tanstack/react-start/client-rpc\"; ";

    fn client(code: &str, rel: &str) -> String {
        rewrite_client(code, rel)
            .expect("rewrites")
            .expect("has a server fn")
    }

    fn rpc(rel: &str, name: &str) -> String {
        format!("createClientRpc({:?})", server_fn_id(rel, name))
    }

    /// The client output must still parse as a module.
    fn parses(out: &str, rel: &str) {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, out, SourceType::from_path(rel).unwrap()).parse();
        assert!(
            parsed.diagnostics.is_empty(),
            "{:?}\n{out}",
            parsed.diagnostics
        );
    }

    fn lines(s: &str) -> usize {
        s.split('\n').count()
    }

    #[test]
    fn ids_match_node_buffer_base64url() {
        assert_eq!(server_fn_id("src/a.ts", "fn"), "c3JjL2EudHMjZm4");
    }

    /// Start 1.171's dev id, byte for byte: `Buffer.from(JSON.stringify({ file:
    /// "/src/server/data.ts?tss-serverfn-split", export:
    /// "getB_createServerFn_handler" })).toString("base64url")` in Node.
    #[test]
    fn vite_dev_ids_match_starts_dev_compiler() {
        assert_eq!(
            vite_dev_server_fn_id("src/server/data.ts", "getB"),
            "eyJmaWxlIjoiL3NyYy9zZXJ2ZXIvZGF0YS50cz90c3Mtc2VydmVyZm4tc3BsaXQiLCJleHBvcnQiOiJnZXRCX2NyZWF0ZVNlcnZlckZuX2hhbmRsZXIifQ"
        );
    }

    // The shape that blanked a preview under the first, textual finder: a
    // comment inside the handler with an unbalanced `)`, where a paren count
    // ended the handler. The handler's span is the parser's, so the whole
    // body goes, comment and all.
    #[test]
    fn a_handler_with_an_unbalanced_paren_in_a_comment_is_replaced_whole() {
        let code = [
            "import { createServerFn } from \"@tanstack/react-start\";",
            "export const saveKey = createServerFn({ method: \"POST\" })",
            "  .handler(async ({ data }) => {",
            "    // 1) chave já gravada localmente para este ambiente",
            "    /* 2) senão, grava :) */",
            "    return data;",
            "  });",
            "export const after = 1;",
            "",
        ]
        .join("\n");
        let rel = "src/qitech.functions.ts";
        let out = client(&code, rel);
        assert!(out.starts_with(CLIENT_IMPORT), "{out}");
        assert!(
            out.contains(&format!(".handler({}\n\n\n\n);", rpc(rel, "saveKey"))),
            "{out}"
        );
        for gone in ["chave", "grava", "return data"] {
            assert!(!out.contains(gone), "{gone} left in:\n{out}");
        }
        assert!(out.contains("export const after = 1;"));
        assert_eq!(lines(&out), lines(&code));
        parses(&out, rel);
    }

    // Start's compiler runs its dead-code pass after the rewrite: an import
    // or helper only the handler or a validator used leaves the client, so
    // the client never imports `@tanstack/react-start/server` (which import
    // protection denies there). What the rest of the module uses, and an
    // import that was unused to begin with, stay.
    #[test]
    fn what_only_the_handler_used_leaves_the_client() {
        let code = [
            "import { createServerFn } from \"@tanstack/react-start\";",
            "import { getRequest, setResponseStatus } from \"@tanstack/react-start/server\";",
            "import { z } from \"zod\";",
            "import { shared } from \"./shared\";",
            "import { untouched } from \"./untouched\";",
            "const secretHeader = () => getRequest()?.headers.get(\"x-secret\");",
            "export const ask = createServerFn({ method: \"POST\" })",
            "  .inputValidator(z.object({ q: z.string() }))",
            "  .handler(async ({ data }) => {",
            "    setResponseStatus(200);",
            "    return shared(data.q) + secretHeader();",
            "  });",
            "export const label = shared(\"label\");",
            "",
        ]
        .join("\n");
        let rel = "src/lib/ask.functions.ts";
        let out = client(&code, rel);
        for gone in [
            "@tanstack/react-start/server",
            "secretHeader",
            "zod",
            "getRequest",
        ] {
            assert!(!out.contains(gone), "{gone} left in:\n{out}");
        }
        for kept in [
            "import { createServerFn } from \"@tanstack/react-start\";",
            "import { shared } from \"./shared\";",
            "import { untouched } from \"./untouched\";",
            "export const label = shared(\"label\");",
        ] {
            assert!(out.contains(kept), "{kept} is gone:\n{out}");
        }
        assert_eq!(lines(&out), lines(&code));
        parses(&out, rel);
    }

    // Review of the dead-code pass: a recursive helper and a destructured
    // binding that only the handler used leave the client with it, as
    // babel-dead-code-elimination drops them (findRemovableBindings, the
    // pattern visitors), so the server entry is never imported there.
    #[test]
    fn a_recursive_or_destructured_helper_only_the_handler_used_leaves_the_client() {
        let code = [
            "import { createServerFn } from \"@tanstack/react-start\";",
            "import * as S from \"@tanstack/react-start/server\";",
            "const { getRequest } = S;",
            "function hdr(n: string, d = 0): string | null {",
            "  return d > 2 ? null : getRequest()?.headers.get(n) ?? hdr(n, d + 1);",
            "}",
            "export const ask = createServerFn().handler(async () => hdr(\"x\"));",
            "",
        ]
        .join("\n");
        let rel = "src/lib/ask.functions.ts";
        let out = client(&code, rel);
        for gone in ["@tanstack/react-start/server", "getRequest", "hdr"] {
            assert!(!out.contains(gone), "{gone} left in:\n{out}");
        }
        assert_eq!(lines(&out), lines(&code));
        parses(&out, rel);
    }

    // Brackets inside strings, templates and regex literals are the handler's
    // own: its span covers them.
    #[test]
    fn brackets_in_strings_templates_and_regexes_are_part_of_the_handler() {
        let code = [
            "export const run = createServerFn()",
            "  .handler(async () => {",
            "    const a = \")((\";",
            "    const b = `x ${f(')')} )`;",
            "    if (/^[)]+\\)/.test(a)) return b;",
            "    return a;",
            "  });",
            "",
        ]
        .join("\n");
        let out = client(&code, "src/run.ts");
        assert!(
            out.contains(&format!(".handler({}", rpc("src/run.ts", "run"))),
            "{out}"
        );
        assert!(!out.contains("return a") && !out.contains(")(("), "{out}");
        parses(&out, "src/run.ts");
    }

    // A `/` right after a template literal divides. The first finder read it
    // as a regex start and lost the chain, so the function went unregistered
    // on the server and in the resolver; the parser knows the difference.
    #[test]
    fn a_division_after_a_template_literal_in_a_validator_keeps_the_chain() {
        let code = [
            "export const half = createServerFn({ method: `GET` })",
            "  .inputValidator((n: number) => `${n}`.length / 2)",
            "  .handler(async ({ data }) => data);",
            "",
        ]
        .join("\n");
        let found: Vec<String> = find_server_fns(&code, "src/half.ts")
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(found, ["half"]);
        // A division right after an opener.
        let code = code.replace("`${n}`.length / 2", "(`4`/2) * n");
        let found: Vec<String> = find_server_fns(&code, "src/half.ts")
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(found, ["half"], "a division right after the template");
    }

    // handleCreateServerFn replaces only handlerFnPath, the first argument.
    #[test]
    fn only_the_first_handler_argument_becomes_the_stub() {
        let out = client(
            "export const x = createServerFn().handler(async () => 1, extra);",
            "src/x.ts",
        );
        assert!(
            out.ends_with(&format!(".handler({}, extra);", rpc("src/x.ts", "x"))),
            "{out}"
        );
    }

    #[test]
    fn validator_and_middleware_chains_find_their_handler() {
        let code = [
            "export const getUser = createServerFn({",
            "  method: \"GET\",",
            "})",
            "  .middleware([auth]) // trailing ) comment",
            "  .validator((s) => {",
            "    if (!/^[(]/.test(s.name) && s.label !== \")((\") return s;",
            "    throw new Error(`bad ${s.name}; (unbalanced`);",
            "  })",
            "  .handler(async ({ data }) => data);",
            "",
        ]
        .join("\n");
        let names: Vec<_> = find_server_fns(&code, "src/user.ts")
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["getUser"]);
        let out = client(&code, "src/user.ts");
        assert!(
            out.contains(&format!(".handler({});", rpc("src/user.ts", "getUser"))),
            "{out}"
        );
        // stripMethodCall: the validator never reaches the browser.
        assert!(
            !out.contains("validator") && !out.contains("unbalanced"),
            "{out}"
        );
        assert!(out.contains(".middleware([auth])"), "{out}");
        assert_eq!(lines(&out), lines(&code));
        parses(&out, "src/user.ts");
    }

    #[test]
    fn input_validator_is_stripped_like_validator() {
        let code =
            "export const v = createServerFn().inputValidator((d) => d).handler(async () => 1);\n";
        let out = client(code, "src/v.ts");
        assert!(
            out.contains(&format!(
                "createServerFn().handler({});",
                rpc("src/v.ts", "v")
            )),
            "{out}"
        );
    }

    // Start's compiler resolves `factory` through its binding to the call
    // that made it a Builder (`resolveIdentifierKind`), so `factory.handler(..)`
    // is a server function named after its own variable, on both sides; the
    // factory variable itself is not one.
    #[test]
    fn a_factory_held_in_a_variable_is_resolved_on_both_sides() {
        let split = "const factory = createServerFn({ method: \"POST\" });\nexport const doIt = factory.handler(async () => 1);\n";
        let names = |code: &str| -> Vec<String> {
            find_server_fns(code, "src/split.ts")
                .into_iter()
                .map(|f| f.name)
                .collect()
        };
        assert_eq!(names(split), ["doIt"]);
        let out = client(split, "src/split.ts");
        assert!(
            out.contains(&format!(
                "factory.handler({});",
                rpc("src/split.ts", "doIt")
            )),
            "{out}"
        );
        let server = rewrite_server(split, "src/split.ts").unwrap();
        assert!(
            server.contains("export const doIt_createServerFn_handler = createServerRpc("),
            "{server}"
        );
        assert!(
            server.contains("factory.handler(doIt_createServerFn_handler, async () => 1)"),
            "{server}"
        );
        assert!(
            !server.contains("factory_createServerFn_handler"),
            "{server}"
        );
        let asi = "const factory = createServerFn({ method: \"POST\" })\nexport const doIt = factory.handler(async () => 1)\n";
        assert_eq!(names(asi), ["doIt"]);

        // A chain through two variables, declared after their use. The
        // compiler's method-chain walk stops at the variable, so a validator
        // link on the way stays, as it does in Start.
        let chained = "export const doIt = checked.handler(async () => 1);\nconst checked = factory.validator((d) => d);\nconst factory = createServerFn();\n";
        assert_eq!(names(chained), ["doIt"]);
        let out = client(chained, "src/chained.ts");
        assert!(
            out.contains(&format!(
                "checked.handler({});",
                rpc("src/chained.ts", "doIt")
            )),
            "{out}"
        );
        assert!(
            out.contains("const checked = factory.validator((d) => d);"),
            "{out}"
        );

        let mixed = "const factory = createServerFn({ method: \"POST\" });\nexport const real = createServerFn({ method: \"GET\" }).handler(async () => 2);\nexport const viaFactory = factory.handler(async () => 3);\n";
        assert_eq!(names(mixed), ["real", "viaFactory"]);
        let server = rewrite_server(mixed, "src/mixed.ts").unwrap();
        assert!(server.contains(".handler(real_createServerFn_handler, async () => 2)"));
        assert!(server.contains(".handler(viaFactory_createServerFn_handler, async () => 3)"));
    }

    // Start's compiler finds a candidate in any variable declaration
    // (`t.isVariableDeclaration`, whatever its kind), and names the function
    // after the declarator's identifier, type annotation and all. The
    // textual finder knew `const NAME =` only, so these shipped their
    // handlers, and the server import inside, to the browser.
    #[test]
    fn a_type_annotated_declaration_is_a_server_fn() {
        let code = [
            "import { createServerFn, type ServerFn } from \"@tanstack/react-start\";",
            "type Input = { q: string };",
            "export const plan: ServerFn<Input> = createServerFn({ method: \"POST\" })",
            "  .inputValidator((d: Input) => d)",
            "  .handler(async ({ data }) => {",
            "    const { gate } = await import(\"./ai-gate.server\");",
            "    return gate(data.q);",
            "  });",
            "",
        ]
        .join("\n");
        let rel = "src/lib/plan.functions.ts";
        let names: Vec<_> = find_server_fns(&code, rel)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["plan"]);
        let out = client(&code, rel);
        assert!(
            out.contains(&format!(
                "export const plan: ServerFn<Input> = createServerFn({{ method: \"POST\" }})\n\n  .handler({}",
                rpc(rel, "plan")
            )),
            "{out}"
        );
        for gone in ["ai-gate.server", "gate(", "inputValidator"] {
            assert!(!out.contains(gone), "{gone} left in:\n{out}");
        }
        assert_eq!(lines(&out), lines(&code));
        parses(&out, rel);
        let server = rewrite_server(&code, rel).unwrap();
        assert!(
            server.contains("export const plan_createServerFn_handler = createServerRpc("),
            "{server}"
        );
        assert!(
            server.contains(".handler(plan_createServerFn_handler, async ({ data }) => {"),
            "{server}"
        );
        assert_eq!(lines(&server), lines(&code));
    }

    #[test]
    fn let_var_and_unexported_declarations_are_server_fns() {
        let code = [
            "import { createServerFn } from \"@tanstack/react-start\";",
            "export let a = createServerFn().handler(async () => \"a-secret\");",
            "var b = createServerFn().handler(async () => \"b-secret\");",
            "const c = createServerFn().handler(async () => \"c-secret\");",
            "export const call = () => Promise.all([a(), b(), c()]);",
            "",
        ]
        .join("\n");
        let rel = "src/abc.ts";
        let names: Vec<_> = find_server_fns(&code, rel)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["a", "b", "c"]);
        let out = client(&code, rel);
        for name in ["a", "b", "c"] {
            assert!(
                out.contains(&format!(".handler({});", rpc(rel, name))),
                "{out}"
            );
            assert!(!out.contains(&format!("{name}-secret")), "{out}");
        }
        parses(&out, rel);
        let server = rewrite_server(&code, rel).unwrap();
        assert!(
            server.contains("export const b_createServerFn_handler = createServerRpc({ id: \"c3JjL2FiYy50cyNi\", name: \"b\", filename: \"src/abc.ts\" }, (opts) => b.__executeServer(opts)); var b = createServerFn().handler(b_createServerFn_handler, async () => \"b-secret\");"),
            "{server}"
        );
        assert!(
            server.contains("export let a = createServerFn().handler(a_createServerFn_handler, "),
            "{server}"
        );
        assert!(
            server.contains("const c = createServerFn().handler(c_createServerFn_handler, "),
            "{server}"
        );
    }

    // Start's `handleCreateServerFn` takes the function's name from the
    // declarator the `.handler(..)` call initializes
    // (`getVariableDeclaratorForExpressionPath`) and has none for a property
    // or a `return`; its fast scan over the module's declarations never
    // reaches them. Neither side treats them as server functions, so the
    // client and the server agree.
    #[test]
    fn a_handler_in_an_object_literal_or_a_return_is_not_one() {
        let shapes = [
            "export const api = { plan: createServerFn().handler(async () => 1) };\n",
            "function make() { return createServerFn().handler(async () => 1); }\nexport const made = make();\n",
            "export const make = () => createServerFn().handler(async () => 1);\n",
            "export const x = createServerFn().handler(async () => 1).extra();\n",
        ];
        for shape in shapes {
            let code =
                format!("import {{ createServerFn }} from \"@tanstack/react-start\";\n{shape}");
            assert!(find_server_fns(&code, "src/o.ts").is_empty(), "{shape}");
            assert_eq!(rewrite_client(&code, "src/o.ts"), Ok(None), "{shape}");
            assert_eq!(rewrite_server(&code, "src/o.ts"), None, "{shape}");
        }
    }

    // The compiler knows the factory through the import binding
    // (`resolveImportKind`), whatever the module calls it, and through a
    // namespace import's member (`resolveCalleeKind`).
    #[test]
    fn the_factory_under_another_name_or_a_namespace_is_resolved() {
        let renamed = [
            "import { createServerFn as fn } from \"@tanstack/react-start\";",
            "export const a = fn({ method: \"POST\" }).handler(async () => \"a-secret\");",
            "",
        ]
        .join("\n");
        let out = client(&renamed, "src/r.ts");
        assert!(
            out.contains(&format!(
                "fn({{ method: \"POST\" }}).handler({});",
                rpc("src/r.ts", "a")
            )),
            "{out}"
        );
        assert!(!out.contains("a-secret"), "{out}");
        assert!(rewrite_server(&renamed, "src/r.ts")
            .unwrap()
            .contains("fn({ method: \"POST\" }).handler(a_createServerFn_handler, "),);

        let namespace = [
            "import * as start from \"@tanstack/react-start\";",
            "export const b = start.createServerFn().validator((d) => d).handler(async () => \"b-secret\");",
            "",
        ]
        .join("\n");
        let out = client(&namespace, "src/n.ts");
        assert!(
            out.contains(&format!(
                "start.createServerFn().handler({});",
                rpc("src/n.ts", "b")
            )),
            "{out}"
        );
        assert!(!out.contains("b-secret"), "{out}");
        assert!(rewrite_server(&namespace, "src/n.ts").unwrap().contains(
            "start.createServerFn().validator((d) => d).handler(b_createServerFn_handler, "
        ),);

        // Another namespace's member of that name is not the factory, nor is
        // an unrelated import called `fn`.
        let other = "import * as mine from \"./mine\";\nimport { fn } from \"./fn\";\nexport const c = mine.other().handler(async () => 1);\nexport const d = fn().handler(async () => 2);\n";
        assert!(find_server_fns(other, "src/m.ts").is_empty());
    }

    // `unwrapExpression`: TypeScript's `as`, `satisfies`, `!` and parentheses
    // around the chain are looked through, and a type argument on the
    // handler call keeps the edit on its first argument.
    #[test]
    fn ts_wrappers_around_the_chain_are_looked_through() {
        let code = [
            "import { createServerFn } from \"@tanstack/react-start\";",
            "export const a = createServerFn().handler(async () => \"a-secret\") as unknown as Fn;",
            "export const b = (createServerFn().validator((d) => d)).handler(async () => \"b-secret\") satisfies Fn;",
            "export const c = createServerFn().handler<Ctx>(async () => \"c-secret\")!;",
            "",
        ]
        .join("\n");
        let rel = "src/w.ts";
        let names: Vec<_> = find_server_fns(&code, rel)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["a", "b", "c"]);
        let out = client(&code, rel);
        assert!(
            out.contains(&format!(
                "createServerFn().handler({}) as unknown as Fn;",
                rpc(rel, "a")
            )),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "(createServerFn()).handler({}) satisfies Fn;",
                rpc(rel, "b")
            )),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "createServerFn().handler<Ctx>({})!;",
                rpc(rel, "c")
            )),
            "{out}"
        );
        assert!(!out.contains("-secret"), "{out}");
        parses(&out, rel);
        let server = rewrite_server(&code, rel).unwrap();
        assert!(
            server.contains(".handler<Ctx>(c_createServerFn_handler, async () => \"c-secret\")!;"),
            "{server}"
        );
    }

    #[test]
    fn a_commented_out_server_fn_is_not_one() {
        let code =
            "// export const old = createServerFn().handler(async () => 1);\nexport const y = 2;\n";
        assert!(find_server_fns(code, "src/y.ts").is_empty());
        assert_eq!(rewrite_client(code, "src/y.ts"), Ok(None));
    }

    #[test]
    fn the_server_rewrite_adds_the_provider_export_without_lines() {
        let code = "import { createServerFn } from \"@tanstack/react-start\";\nexport const getGreeting = createServerFn({ method: \"GET\" }).handler(async () => \"hi\");\n";
        let out = rewrite_server(code, "src/server/data.ts").unwrap();
        let id = server_fn_id("src/server/data.ts", "getGreeting");
        assert!(out
            .starts_with("import { createServerRpc } from \"@tanstack/react-start/server-rpc\"; "));
        assert!(
            out.contains(&format!(
                "export const getGreeting_createServerFn_handler = createServerRpc({{ id: \"{id}\", name: \"getGreeting\", filename: \"src/server/data.ts\" }}, (opts) => getGreeting.__executeServer(opts)); "
            )),
            "{out}"
        );
        assert!(out.contains(".handler(getGreeting_createServerFn_handler, async () => \"hi\")"));
        assert_eq!(lines(&out), lines(code));
    }

    #[test]
    fn adjacent_server_fns_rewrite_independently() {
        let code = "export const a = createServerFn({ method: \"GET\" }).handler(async () => 1);\nexport const b = createServerFn({ method: \"POST\" }).handler(async () => 2);\n";
        let out = rewrite_server(code, "src/two.ts").unwrap();
        for name in ["a", "b"] {
            let id = server_fn_id("src/two.ts", name);
            assert!(
                out.contains(&format!(
                    "export const {name}_createServerFn_handler = createServerRpc({{ id: \"{id}\""
                )),
                "{out}"
            );
        }
        assert!(out.contains(".handler(a_createServerFn_handler, async () => 1)"));
        assert!(out.contains(".handler(b_createServerFn_handler, async () => 2)"));
        assert_eq!(lines(&out), lines(code));
        let client_out = client(code, "src/two.ts");
        assert!(client_out.contains(&rpc("src/two.ts", "a")));
        assert!(client_out.contains(&rpc("src/two.ts", "b")));
    }

    #[test]
    fn the_server_rewrite_keeps_a_commented_handler_body_intact() {
        let code =
            "export const k = createServerFn().handler(async () => {\n  // 1) chave\n  return 1;\n});\n";
        let out = rewrite_server(code, "src/k.ts").unwrap();
        assert!(
            out.contains(
                ".handler(k_createServerFn_handler, async () => {\n  // 1) chave\n  return 1;\n});"
            ),
            "{out}"
        );
    }

    #[test]
    fn modules_without_a_server_fn_pass_through() {
        assert_eq!(rewrite_client("export const x = 1;", "src/x.ts"), Ok(None));
        assert_eq!(rewrite_server("export const x = 1;", "src/x.ts"), None);
    }

    // A regex after a keyword, a `(` in JSX text: the first finder could not
    // delimit these bodies and shipped the handler to the browser. The
    // handler's span covers them.
    #[test]
    fn handler_bodies_with_a_keyword_regex_or_jsx_parens_are_replaced() {
        let cases = [
            (
                "src/re.ts",
                "re",
                "export const re = createServerFn().handler(async () => {\n  return /\\)/.test(\"x\");\n});\n",
            ),
            (
                "src/th.ts",
                "th",
                "export const th = createServerFn().handler(async ({ data }) => {\n  if (data) throw /x\\(/;\n  return 1;\n});\n",
            ),
            (
                "src/jsx.tsx",
                "jsx",
                "export const jsx = createServerFn().handler(async () => {\n  return <p>(note</p>;\n});\n",
            ),
        ];
        for (rel, name, code) in cases {
            let out = client(code, rel);
            assert!(
                out.contains(&format!(".handler({}", rpc(rel, name))),
                "{out}"
            );
            for gone in ["return", "throw", "<p>"] {
                assert!(!out.contains(gone), "{gone} left in:\n{out}");
            }
            assert_eq!(lines(&out), lines(code), "{out}");
        }
    }

    // TanStack unshifts its import into the program body, after the directives.
    #[test]
    fn the_import_lands_after_the_directive_prologue_on_its_line() {
        let code = "\"use client\";\nexport const d = createServerFn().handler(async () => 1);\n";
        let out = client(code, "src/d.ts");
        assert!(
            out.starts_with(&format!("\"use client\"; {CLIENT_IMPORT}")),
            "{out}"
        );
        assert_eq!(lines(&out), lines(code));
        let hashbang =
            "#!/usr/bin/env node\nexport const h = createServerFn().handler(async () => 1);\n";
        let out = client(hashbang, "src/h.ts");
        assert!(
            out.starts_with(&format!("#!/usr/bin/env node\n{CLIENT_IMPORT}")),
            "{out}"
        );
    }

    // Never left in place: an unrewritten server fn would ship its handler.
    #[test]
    fn a_server_fn_the_client_rewrite_cannot_handle_fails_loudly() {
        let err = rewrite_client("export const e = createServerFn().handler();\n", "src/e.ts")
            .unwrap_err();
        assert!(
            err.0.contains("could not rewrite server function e"),
            "{err}"
        );
    }

    // Byte offsets, not UTF-16: a multi-byte character before the handler
    // must not shift the matched call.
    #[test]
    fn multi_byte_source_before_the_handler_keeps_the_edit_aligned() {
        let code = "// ação — 🚀\nexport const u = createServerFn().handler(async () => \"é\");\n";
        let out = client(code, "src/u.ts");
        assert!(
            out.contains(&format!(".handler({});", rpc("src/u.ts", "u"))),
            "{out}"
        );
        parses(&out, "src/u.ts");
    }

    /// The app's modules by path, a relative specifier resolved as a `.ts`
    /// file next to the importer.
    struct MapModules(Vec<(&'static str, &'static str)>);

    impl Modules for MapModules {
        fn read(&self, from: &str, specifier: &str) -> Option<(String, String)> {
            if !specifier.starts_with('.') {
                return None;
            }
            let dir = Path::new(from).parent()?;
            let path = format!("{}.ts", slash(&normalize(&dir.join(specifier))));
            self.0
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(p, code)| (p.to_string(), code.to_string()))
        }
    }

    // Start's compiler follows an import to the binding that made it a
    // Builder (`resolveImportKind`, `findExportInModule` through `export {}`
    // and `export * from`), and reads a module for `.handler(` alone
    // (`KindDetectionPatterns.ServerFn`), so a server function whose builder
    // another module exports is one, named after its own variable, on both
    // sides. The builder's module declares none.
    #[test]
    fn a_builder_another_module_exports_is_resolved_on_both_sides() {
        let modules = MapModules(vec![
            (
                "src/lib/base.ts",
                "import { createServerFn } from \"@tanstack/react-start\";\nexport const base = createServerFn({ method: \"POST\" }).middleware([]);\nconst guarded = base.validator((d: unknown) => d);\nexport { guarded as checked };\nexport const done = base.handler(async () => 1);\n",
            ),
            (
                "src/lib/again.ts",
                "export { base } from \"./base\";\nexport * from \"./base\";\n",
            ),
            (
                "src/lib/start.ts",
                "export { createServerFn } from \"@tanstack/react-start\";\nimport { createServerFn as make } from \"@tanstack/react-start\";\nexport const factory = make;\n",
            ),
        ]);
        // No `createServerFn` in the text: the handler call alone reads it.
        let code = [
            "import { base, checked } from \"../lib/base\";",
            "import * as fns from \"../lib/again\";",
            "export const x = base.handler(async () => \"x-secret\");",
            "export const y = checked.validator((d) => d).handler(async () => \"y-secret\");",
            "export const z = fns.base.handler(async () => \"z-secret\");",
            "export const w = fns.checked.handler(async () => \"w-secret\");",
            "",
        ]
        .join("\n");
        let rel = "src/routes/fns.ts";
        let names: Vec<_> = super::find_server_fns(&code, rel, &modules)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["x", "y", "z", "w"]);
        let out = super::rewrite_client(&code, rel, &modules)
            .unwrap()
            .unwrap();
        for name in ["x", "y", "z", "w"] {
            assert!(
                out.contains(&format!(".handler({});", rpc(rel, name))),
                "{out}"
            );
        }
        assert!(
            out.contains(&format!(
                "export const y = checked.handler({});",
                rpc(rel, "y")
            )),
            "{out}"
        );
        assert!(!out.contains("-secret"), "{out}");
        assert_eq!(lines(&out), lines(&code));
        parses(&out, rel);
        let server = super::rewrite_server(&code, rel, &modules).unwrap();
        for name in ["x", "y", "z", "w"] {
            assert!(
                server.contains(&format!(
                    "export const {name}_createServerFn_handler = createServerRpc("
                )),
                "{server}"
            );
        }
        assert!(
            server.contains("fns.checked.handler(w_createServerFn_handler, "),
            "{server}"
        );

        // The factory itself through the app's module: as the module
        // re-exports it, and as a variable the module binds it to.
        let code = [
            "import { createServerFn as fn, factory } from \"../lib/start\";",
            "export const a = fn().handler(async () => \"a-secret\");",
            "export const b = factory({ method: \"GET\" }).handler(async () => \"b-secret\");",
            "",
        ]
        .join("\n");
        let names: Vec<_> = super::find_server_fns(&code, rel, &modules)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["a", "b"]);

        // The builder's own module: `base` and `checked` are builders, not
        // functions; `done` is one of its own.
        let (_, base) = modules.read("src/routes/fns.ts", "../lib/base").unwrap();
        let names: Vec<_> = super::find_server_fns(&base, "src/lib/base.ts", &modules)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["done"]);
    }

    // An import that is not a builder, a module the app does not hold, and a
    // cycle of re-exports: none is a server function, and the walk ends.
    #[test]
    fn an_import_that_is_no_builder_is_not_a_server_fn() {
        let modules = MapModules(vec![
            (
                "src/other.ts",
                "export const other = { handler: (f: () => unknown) => f };\nexport const fromPkg = somePackageThing();\n",
            ),
            ("src/a.ts", "export { b as a } from \"./b\";\n"),
            ("src/b.ts", "export { a as b } from \"./a\";\nexport * from \"./a\";\n"),
        ]);
        let code = [
            "import { other, fromPkg } from \"./other\";",
            "import { missing } from \"./missing\";",
            "import { a } from \"./a\";",
            "import { handler } from \"some-package\";",
            "export const p = other.handler(async () => 1);",
            "export const q = fromPkg.handler(async () => 1);",
            "export const r = missing.handler(async () => 1);",
            "export const s = a.handler(async () => 1);",
            "export const t = handler.handler(async () => 1);",
            "",
        ]
        .join("\n");
        assert!(super::find_server_fns(&code, "src/fns.ts", &modules).is_empty());
        assert_eq!(
            super::rewrite_client(&code, "src/fns.ts", &modules),
            Ok(None)
        );
        assert_eq!(super::rewrite_server(&code, "src/fns.ts", &modules), None);
    }

    // A builder another module exports as its default: `import base from
    // "./base"` with `export default createServerFn()`, `export default base`
    // (a variable holding one) or `export { base as default }` there, as the
    // compiler's `findExportInModule` reads `default` like any other name.
    // `export default` of a function is no builder.
    #[test]
    fn a_builder_another_module_exports_as_default_is_resolved() {
        let modules = MapModules(vec![
            (
                "src/lib/direct.ts",
                "import { createServerFn } from \"@tanstack/react-start\";\nexport default createServerFn({ method: \"POST\" });\n",
            ),
            (
                "src/lib/named.ts",
                "import { createServerFn } from \"@tanstack/react-start\";\nconst base = createServerFn({ method: \"POST\" }).middleware([]);\nexport default base;\n",
            ),
            (
                "src/lib/listed.ts",
                "import { createServerFn } from \"@tanstack/react-start\";\nconst base = createServerFn();\nexport { base as default };\n",
            ),
            (
                "src/lib/plain.ts",
                "export default function handler(f: () => unknown) {\n  return f;\n}\n",
            ),
        ]);
        let code = [
            "import direct from \"../lib/direct\";",
            "import named from \"../lib/named\";",
            "import listed from \"../lib/listed\";",
            "import plain from \"../lib/plain\";",
            "export const a = direct.handler(async () => \"a-secret\");",
            "export const b = named.handler(async () => \"b-secret\");",
            "export const c = listed.handler(async () => \"c-secret\");",
            "export const d = plain.handler(async () => \"d-kept\");",
            "",
        ]
        .join("\n");
        let rel = "src/routes/fns.ts";
        let names: Vec<_> = super::find_server_fns(&code, rel, &modules)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["a", "b", "c"]);
        let out = super::rewrite_client(&code, rel, &modules)
            .unwrap()
            .unwrap();
        for name in ["a", "b", "c"] {
            assert!(
                out.contains(&format!(".handler({});", rpc(rel, name))),
                "{out}"
            );
        }
        assert!(!out.contains("-secret"), "{out}");
        assert!(out.contains("d-kept"), "{out}");
        parses(&out, rel);
        let server = super::rewrite_server(&code, rel, &modules).unwrap();
        assert!(
            server.contains("named.handler(b_createServerFn_handler, "),
            "{server}"
        );
    }

    // A builder imported through an alias the app's resolver knows: a
    // tsconfig `paths` entry and a package.json `imports` entry, which
    // Start's compiler resolves with Vite's `this.resolve`. Both sides find
    // the function, so the client calls the id the server registers.
    #[test]
    fn a_builder_imported_through_an_alias_is_resolved_on_both_sides() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("oj-server-fns-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, code) in [
            (
                "tsconfig.json",
                "{ \"compilerOptions\": { \"baseUrl\": \".\", \"paths\": { \"@app/*\": [\"./src/*\"] } } }\n",
            ),
            (
                "package.json",
                "{ \"name\": \"app\", \"type\": \"module\", \"imports\": { \"#lib/*\": \"./src/lib/*\" } }\n",
            ),
            (
                "src/lib/base.ts",
                "import { createServerFn } from \"@tanstack/react-start\";\nexport const base = createServerFn({ method: \"POST\" });\nexport default base;\n",
            ),
            ("src/lib/kit.ts", "export { createServerFn } from \"@tanstack/react-start\";\n"),
            (
                "node_modules/pkg/index.js",
                "export const base = { handler: (f) => f };\n",
            ),
        ] {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, code).unwrap();
        }
        let modules = FsModules::new(&root);
        let from = "src/routes/fns.ts";
        let read = |specifier: &str| modules.read(from, specifier).map(|(path, _)| path);
        assert_eq!(read("@app/lib/base").as_deref(), Some("src/lib/base.ts"));
        assert_eq!(read("#lib/base").as_deref(), Some("src/lib/base.ts"));
        assert_eq!(read("#lib/base.ts").as_deref(), Some("src/lib/base.ts"));
        assert_eq!(read("@app/lib/missing"), None);
        // A package is not the app's, installed or not.
        assert_eq!(read("pkg"), None);
        assert_eq!(read("@tanstack/react-start"), None);

        let code = [
            "import { base } from \"@app/lib/base\";",
            "import viaImports from \"#lib/base\";",
            "import { createServerFn as make } from \"#lib/kit\";",
            "import * as lib from \"@app/lib/base\";",
            "import { base as pkgBase } from \"pkg\";",
            "export const a = base.handler(async () => \"a-secret\");",
            "export const b = viaImports.handler(async () => \"b-secret\");",
            "export const c = make().handler(async () => \"c-secret\");",
            "export const d = lib.base.handler(async () => \"d-secret\");",
            "export const e = pkgBase.handler(async () => \"e-kept\");",
            "",
        ]
        .join("\n");
        let names: Vec<_> = super::find_server_fns(&code, from, &modules)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["a", "b", "c", "d"]);
        let out = super::rewrite_client(&code, from, &modules)
            .unwrap()
            .unwrap();
        for name in ["a", "b", "c", "d"] {
            assert!(
                out.contains(&format!(".handler({});", rpc(from, name))),
                "{out}"
            );
        }
        assert!(!out.contains("-secret"), "{out}");
        assert!(out.contains("e-kept"), "{out}");
        parses(&out, from);
        let server = super::rewrite_server(&code, from, &modules).unwrap();
        for name in ["a", "b", "c", "d"] {
            assert!(
                server.contains(&format!(
                    "export const {name}_createServerFn_handler = createServerRpc("
                )),
                "{server}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // `FsModules` resolves a relative specifier as Vite's `tryCleanFsResolve`
    // does: the file as written, a JS import's TypeScript source, the default
    // extensions in order, a directory's index; a package is not the app's.
    #[test]
    fn fs_modules_resolve_a_relative_import_as_vite_does() {
        let root = std::env::temp_dir().join(format!("oj-server-fns-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, code) in [
            ("src/lib/base.functions.ts", "export const base = 1;\n"),
            ("src/lib/plain.mjs", "export const plain = 1;\n"),
            ("src/lib/plain.ts", "export const shadowed = 1;\n"),
            ("src/lib/typed.ts", "export const typed = 1;\n"),
            ("src/lib/wide.tsx", "export const wide = 1;\n"),
            ("src/lib/dir/index.ts", "export const dir = 1;\n"),
            ("src/lib/withExt.js", "export const js = 1;\n"),
        ] {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, code).unwrap();
        }
        let modules = FsModules::new(&root);
        let from = "src/routes/index.tsx";
        let read = |specifier: &str| modules.read(from, specifier).map(|(path, _)| path);
        assert_eq!(
            read("../lib/base.functions").as_deref(),
            Some("src/lib/base.functions.ts")
        );
        assert_eq!(
            read("../lib/withExt.js").as_deref(),
            Some("src/lib/withExt.js")
        );
        assert_eq!(read("../lib/typed.js").as_deref(), Some("src/lib/typed.ts"));
        assert_eq!(read("../lib/wide.js").as_deref(), Some("src/lib/wide.tsx"));
        // `.mjs` comes before `.ts` in Vite's extension order.
        assert_eq!(read("../lib/plain").as_deref(), Some("src/lib/plain.mjs"));
        assert_eq!(read("../lib/dir").as_deref(), Some("src/lib/dir/index.ts"));
        assert_eq!(
            read("./../lib/./dir/index").as_deref(),
            Some("src/lib/dir/index.ts")
        );
        assert_eq!(read("../lib/missing"), None);
        assert_eq!(read("@tanstack/react-start"), None);
        assert_eq!(read("some-package/base"), None);
        let (path, code) = modules.read(from, "../lib/base.functions").unwrap();
        assert_eq!(
            (path.as_str(), code.as_str()),
            ("src/lib/base.functions.ts", "export const base = 1;\n")
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
