//! Per-file compile state: semantic model, import tables, collected rules.
// parity: babel-plugin src/utils/state-manager.js (traversal-state subset)

use std::cell::RefCell;

use crate::fxhash::FxHashMap;

use oxc_ast::AstKind;
use oxc_ast::ast::{
    BindingPattern, Expression, ForStatementLeft, IdentifierReference, Program,
    VariableDeclarationKind, VariableDeclarator,
};
use oxc_semantic::{AstNodes, Semantic, SemanticBuilder, Stats};
use oxc_span::{GetSpan, Span};
use oxc_syntax::node::NodeId;
use oxc_syntax::reference::ReferenceId;
use oxc_syntax::symbol::SymbolId;

use crate::errors::StylexError;
use crate::eval::cross_file::VarGroupProxy;
use crate::eval::value::EvalValue;
use crate::imports::{ImportTable, scan_imports};
use crate::options::ResolvedOptions;
use crate::rules::StylexRule;
use crate::scopes::{ScopeBackend, ScopeModel};

/// A style variable the bail path must keep alive (namespace `None` = all).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleVarToKeep {
    pub var_name: String,
    pub namespace: Option<String>,
}

/// One recorded treeshake-compensation insertion site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeshakeImport {
    pub specifier: String,
    /// Start offset of the declaring ImportDeclaration.
    pub decl_start: u32,
    /// Span of that declaration's source literal (for raw-quoted reprint).
    pub source_span: Span,
}

/// What a symbol's declaration site is, shaped the way babel `binding.path`
/// exposes it to `resolve()`/`_evaluate`.
#[derive(Debug, Clone, Copy)]
pub enum BindingDecl<'a> {
    Declarator(&'a VariableDeclarator<'a>),
    NamedImport,
    DefaultImport,
    NamespaceImport,
    /// The babel node-type name evaluating the declaration would report
    /// ("Identifier" for simple params, "FunctionDeclaration", ...).
    Opaque(&'static str),
}

impl BindingDecl<'_> {
    pub fn is_import(&self) -> bool {
        matches!(
            self,
            BindingDecl::NamedImport | BindingDecl::DefaultImport | BindingDecl::NamespaceImport
        )
    }
}

pub struct BindingInfo<'a> {
    pub decl: BindingDecl<'a>,
    /// babel `binding.path.node` span; use-before-declaration compares `end`.
    pub span: Span,
}

/// A reference's parent after climbing parens, as far as the site indexes
/// look: a member expression naming `property`, or anything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefParent<'a> {
    Other,
    Member { property: Option<&'a str> },
}

/// One reference handed to a `reference_starts_where` filter; the parent is
/// computed only when asked for (the oxc path climbs the node table).
#[derive(Clone, Copy)]
pub struct RefSite<'n, 'a> {
    inner: RefSiteInner<'n, 'a>,
}

#[derive(Clone, Copy)]
enum RefSiteInner<'n, 'a> {
    Oxc(&'n AstNodes<'a>, NodeId),
    Native(RefParent<'a>),
}

impl<'a> RefSite<'_, 'a> {
    pub(crate) fn native(parent: RefParent<'a>) -> Self {
        RefSite {
            inner: RefSiteInner::Native(parent),
        }
    }

    pub fn parent(&self) -> RefParent<'a> {
        match self.inner {
            RefSiteInner::Oxc(nodes, node) => ref_parent_of(nodes, node),
            RefSiteInner::Native(parent) => parent,
        }
    }
}

// One per compile; boxing the oxc side would only add an allocation.
#[allow(clippy::large_enum_variant)]
enum Scoping<'a> {
    Oxc(Semantic<'a>),
    Native(ScopeModel<'a>),
}

pub struct CompileState<'a> {
    pub options: &'a ResolvedOptions,
    /// Absolute source filename, '/'-separated.
    pub filename: Option<String>,
    pub cwd: String,
    pub imports: ImportTable,
    scoping: Scoping<'a>,
    /// (babel constantViolations non-empty, evaluate-path.js isMutated) per
    /// symbol, computed on first query: eval touches a handful of symbols.
    constness: RefCell<FxHashMap<SymbolId, (bool, bool)>>,
    pub style_vars_to_keep: Vec<StyleVarToKeep>,
    /// metadata.stylex accumulator in traversal order (integrator appends).
    pub rules: Vec<StylexRule>,
    /// keyframes()/positionTry() injections recorded during evaluation, keyed
    /// by generated name; upstream emits these before the create rules.
    pub other_injected_rules: Vec<StylexRule>,
    /// Treeshake-compensation side-effect imports in first-evaluation order
    /// (upstream addedImports): (specifier, declaring-import span offsets).
    pub treeshake_imports: Vec<TreeshakeImport>,
    /// Values of declarators whose init a visitor already replaced (babel
    /// re-resolves the binding to the replacement node).
    binding_overrides: FxHashMap<SymbolId, EvalValue>,
    /// `import_path_resolver` memo, keyed by specifier (filename + options are
    /// fixed per compile): otherwise every evaluated reference re-walks the fs.
    import_resolutions: FxHashMap<String, Option<String>>,
    /// Theme-file proxies by import symbol: the `seen` cache is per entry, so
    /// every evaluated reference would otherwise re-hash the var group.
    theme_proxies: Vec<(SymbolId, VarGroupProxy)>,
}

impl<'a> CompileState<'a> {
    pub fn build(
        program: &'a Program<'a>,
        options: &'a ResolvedOptions,
        filename: Option<String>,
        cwd: String,
    ) -> Result<Self, StylexError> {
        let imports = {
            let _t = crate::timings::start(crate::timings::Stage::ImportScan);
            scan_imports(program, options)?
        };
        Ok(Self::build_with_imports(
            program, options, filename, cwd, imports,
        ))
    }

    /// [`Self::build`] over an import table the caller already scanned.
    pub fn build_with_imports(
        program: &'a Program<'a>,
        options: &'a ResolvedOptions,
        filename: Option<String>,
        cwd: String,
        imports: ImportTable,
    ) -> Self {
        Self::build_with_imports_using(
            program,
            options,
            filename,
            cwd,
            imports,
            ScopeBackend::from_env(),
        )
    }

    /// [`Self::build_with_imports`] on an explicit scope backend.
    pub fn build_with_imports_using(
        program: &'a Program<'a>,
        options: &'a ResolvedOptions,
        filename: Option<String>,
        cwd: String,
        imports: ImportTable,
        backend: ScopeBackend,
    ) -> Self {
        let _t = crate::timings::start(crate::timings::Stage::Semantic);
        let scoping = match backend {
            ScopeBackend::Oxc => Scoping::Oxc(
                SemanticBuilder::new()
                    .with_stats(estimated_stats(program.source_text.len()))
                    .with_build_nodes(true)
                    .build(program)
                    .semantic,
            ),
            ScopeBackend::Native => Scoping::Native(ScopeModel::build(program)),
        };
        drop(_t);
        CompileState {
            options,
            filename,
            cwd,
            imports,
            scoping,
            constness: RefCell::new(FxHashMap::default()),
            style_vars_to_keep: Vec::new(),
            rules: Vec::new(),
            other_injected_rules: Vec::new(),
            treeshake_imports: Vec::new(),
            binding_overrides: FxHashMap::default(),
            import_resolutions: FxHashMap::default(),
            theme_proxies: Vec::new(),
        }
    }

    pub fn theme_proxy(&self, symbol: SymbolId) -> Option<&VarGroupProxy> {
        self.theme_proxies
            .iter()
            .find(|(s, _)| *s == symbol)
            .map(|(_, proxy)| proxy)
    }

    pub fn record_theme_proxy(&mut self, symbol: SymbolId, proxy: VarGroupProxy) {
        self.theme_proxies.push((symbol, proxy));
    }

    /// Memoized [`crate::module_resolution::import_path_resolver`]: the dep
    /// log and treeshake records both dedup, so a memo hit records identically.
    pub fn resolve_import_path(
        &mut self,
        fs: &dyn crate::module_resolution::FsProvider,
        specifier: &str,
    ) -> Option<String> {
        if let Some(hit) = self.import_resolutions.get(specifier) {
            return hit.clone();
        }
        let resolved = crate::module_resolution::import_path_resolver(
            fs,
            specifier,
            self.filename.as_deref().map(std::path::Path::new),
            self.options,
        );
        self.import_resolutions
            .insert(specifier.to_string(), resolved.clone());
        resolved
    }

    pub fn backend(&self) -> ScopeBackend {
        match &self.scoping {
            Scoping::Oxc(_) => ScopeBackend::Oxc,
            Scoping::Native(_) => ScopeBackend::Native,
        }
    }

    /// babel `path.scope.getBinding(name)` for a reference site.
    pub fn symbol_of(&self, id: &IdentifierReference<'a>) -> Option<SymbolId> {
        match &self.scoping {
            Scoping::Oxc(semantic) => {
                let reference_id = id.reference_id.get()?;
                semantic.scoping().get_reference(reference_id).symbol_id()
            }
            Scoping::Native(model) => model.symbol_of(id),
        }
    }

    /// babel `binding.constantViolations.length > 0` (= `!binding.constant`).
    pub fn is_non_constant(&self, symbol: SymbolId) -> bool {
        self.constness(symbol).0
    }

    /// evaluate-path.js `isMutated(binding)` — resolve() chains bypass this.
    pub fn is_mutated(&self, symbol: SymbolId) -> bool {
        self.constness(symbol).1
    }

    fn constness(&self, symbol: SymbolId) -> (bool, bool) {
        let semantic = match &self.scoping {
            Scoping::Oxc(semantic) => semantic,
            Scoping::Native(model) => return model.constness(symbol),
        };
        if let Some(&flags) = self.constness.borrow().get(&symbol) {
            return flags;
        }
        let flags = non_constant_flags(semantic, symbol);
        self.constness.borrow_mut().insert(symbol, flags);
        flags
    }

    pub fn binding_info(&self, symbol: SymbolId) -> BindingInfo<'a> {
        let semantic = match &self.scoping {
            Scoping::Oxc(semantic) => semantic,
            Scoping::Native(model) => return model.binding_info(symbol),
        };
        let node = semantic.symbol_declaration(symbol);
        let span = node.kind().span();
        match node.kind() {
            AstKind::VariableDeclarator(declarator) => BindingInfo {
                decl: BindingDecl::Declarator(declarator),
                span,
            },
            AstKind::ImportSpecifier(_) => BindingInfo {
                decl: BindingDecl::NamedImport,
                span,
            },
            AstKind::ImportDefaultSpecifier(_) => BindingInfo {
                decl: BindingDecl::DefaultImport,
                span,
            },
            AstKind::ImportNamespaceSpecifier(_) => BindingInfo {
                decl: BindingDecl::NamespaceImport,
                span,
            },
            AstKind::FormalParameter(param) => BindingInfo {
                decl: BindingDecl::Opaque(binding_pattern_type_name(&param.pattern)),
                span,
            },
            AstKind::CatchParameter(_) => {
                // babel registers catch bindings on the whole CatchClause.
                let clause_span = semantic
                    .nodes()
                    .ancestors(node.id())
                    .find_map(|ancestor| match ancestor.kind() {
                        AstKind::CatchClause(clause) => Some(clause.span),
                        _ => None,
                    })
                    .unwrap_or(span);
                BindingInfo {
                    decl: BindingDecl::Opaque("CatchClause"),
                    span: clause_span,
                }
            }
            AstKind::Function(function) => BindingInfo {
                decl: BindingDecl::Opaque(if function.is_expression() {
                    "FunctionExpression"
                } else {
                    "FunctionDeclaration"
                }),
                span,
            },
            AstKind::Class(class) => BindingInfo {
                decl: BindingDecl::Opaque(if class.is_expression() {
                    "ClassExpression"
                } else {
                    "ClassDeclaration"
                }),
                span,
            },
            AstKind::TSEnumDeclaration(_) => BindingInfo {
                decl: BindingDecl::Opaque("TSEnumDeclaration"),
                span,
            },
            _ => BindingInfo {
                decl: BindingDecl::Opaque("Identifier"),
                span,
            },
        }
    }

    pub fn binding_override(&self, symbol: SymbolId) -> Option<&EvalValue> {
        self.binding_overrides.get(&symbol)
    }

    /// Records a replaced-init value for a module-level binding by name
    /// (visitors only replace top-level declarator inits).
    pub fn record_root_override(&mut self, name: &str, value: EvalValue) {
        if let Some(symbol) = self.root_binding(name) {
            self.binding_overrides.insert(symbol, value);
        }
    }

    /// babel `program.scope.getBinding(name)`.
    pub fn root_binding(&self, name: &str) -> Option<SymbolId> {
        match &self.scoping {
            Scoping::Oxc(semantic) => semantic.scoping().get_root_binding(name.into()),
            Scoping::Native(model) => model.root_binding(name),
        }
    }

    /// babel generateUid collision surface: globals plus every declaration in
    /// the file (registerBinding marks program.references even when unused).
    pub fn uid_name_taken(&self, name: &str) -> bool {
        let semantic = match &self.scoping {
            Scoping::Oxc(semantic) => semantic,
            Scoping::Native(model) => return model.uid_name_taken(name),
        };
        let scoping = semantic.scoping();
        if scoping
            .root_unresolved_references()
            .keys()
            .any(|n| n.as_str() == name)
        {
            return true;
        }
        scoping
            .symbol_ids()
            .any(|symbol| scoping.symbol_name(symbol) == name)
    }

    /// Sorted start offsets of every identifier reference named in `names`,
    /// whichever binding it resolves to (the visitors match by name, as upstream).
    pub fn reference_starts(&self, names: &[&str]) -> Vec<u32> {
        self.reference_starts_where(names, |_, _| true)
    }

    /// [`Self::reference_starts`] keeping only the references `keep` accepts,
    /// given the reference's name and site.
    pub fn reference_starts_where(
        &self,
        names: &[&str],
        keep: impl Fn(&str, RefSite<'_, 'a>) -> bool,
    ) -> Vec<u32> {
        let mut starts = Vec::new();
        if names.is_empty() {
            return starts;
        }
        let semantic = match &self.scoping {
            Scoping::Oxc(semantic) => semantic,
            Scoping::Native(model) => return model.reference_starts_where(names, keep),
        };
        let scoping = semantic.scoping();
        let nodes = semantic.nodes();
        let mut push_all = |name: &str, ids: &[ReferenceId]| {
            for &id in ids {
                let node_id = scoping.get_reference(id).node_id();
                let site = RefSite {
                    inner: RefSiteInner::Oxc(nodes, node_id),
                };
                if keep(name, site) {
                    starts.push(nodes.kind(node_id).span().start);
                }
            }
        };
        for symbol in scoping.symbol_ids() {
            let name = scoping.symbol_name(symbol);
            if names.contains(&name) {
                push_all(name, scoping.get_resolved_reference_ids(symbol));
            }
        }
        for (name, ids) in scoping.root_unresolved_references() {
            if names.contains(&name.as_str()) {
                push_all(name.as_str(), ids);
            }
        }
        starts.sort_unstable();
        starts
    }

    /// Whether `name` at the scope holding `node` resolves to the given
    /// program-level symbol (babel `path.scope.getBinding(n) === programBinding`).
    pub fn resolves_to_root_binding(&self, node_id: NodeId, name: &str) -> bool {
        let semantic = match &self.scoping {
            Scoping::Oxc(semantic) => semantic,
            Scoping::Native(model) => return model.resolves_to_root_binding(node_id, name),
        };
        let scoping = semantic.scoping();
        let Some(root_symbol) = scoping.get_root_binding(name.into()) else {
            return false;
        };
        let scope = semantic.nodes().get_node(node_id).scope_id();
        scoping.find_binding(scope, name.into()) == Some(root_symbol)
    }

    // parity: ast-helpers.js isProgramLevel — false when the walk to Program
    // crosses a function or a statement hanging off anything but Program/export.
    pub fn is_program_level(&self, node_id: NodeId) -> bool {
        let semantic = match &self.scoping {
            Scoping::Oxc(semantic) => semantic,
            Scoping::Native(model) => return model.is_program_level(node_id),
        };
        let nodes = semantic.nodes();
        let mut current = nodes.get_node(node_id);
        for parent in nodes.ancestors(node_id) {
            let parent_kind = parent.kind();
            if babel_is_statement(current.kind())
                && !matches!(parent_kind, AstKind::Program(_))
                && !babel_is_export_declaration(parent_kind)
            {
                return false;
            }
            if current.kind().is_function_like() {
                return false;
            }
            current = parent;
        }
        true
    }

    /// babel getProgramStatement: start offset of the top-level statement
    /// containing `node` (hoisted consts insert before it).
    pub fn program_statement_start(&self, node_id: NodeId) -> u32 {
        let semantic = match &self.scoping {
            Scoping::Oxc(semantic) => semantic,
            Scoping::Native(model) => return model.program_statement_start(node_id),
        };
        let nodes = semantic.nodes();
        let mut current = nodes.get_node(node_id);
        for parent in nodes.ancestors(node_id) {
            if matches!(parent.kind(), AstKind::Program(_)) {
                break;
            }
            current = parent;
        }
        current.kind().span().start
    }

    /// babel `path.scope.hasBinding(name)` at the scope holding `node`.
    pub fn any_binding_at(&self, node_id: NodeId, name: &str) -> bool {
        let semantic = match &self.scoping {
            Scoping::Oxc(semantic) => semantic,
            Scoping::Native(model) => return model.any_binding_at(node_id, name),
        };
        let scope = semantic.nodes().get_node(node_id).scope_id();
        semantic
            .scoping()
            .find_binding(scope, name.into())
            .is_some()
    }

    /// Every symbol as (name, babel decl type, declaring span), unordered.
    #[cfg(test)]
    pub(crate) fn debug_symbols(&self) -> Vec<(String, &'static str, Span)> {
        let label = |info: BindingInfo<'_>| match info.decl {
            BindingDecl::Declarator(_) => "VariableDeclarator",
            BindingDecl::NamedImport => "NamedImport",
            BindingDecl::DefaultImport => "DefaultImport",
            BindingDecl::NamespaceImport => "NamespaceImport",
            BindingDecl::Opaque(type_name) => type_name,
        };
        match &self.scoping {
            Scoping::Oxc(semantic) => {
                let scoping = semantic.scoping();
                scoping
                    .symbol_ids()
                    .map(|s| {
                        let info = self.binding_info(s);
                        let span = info.span;
                        (scoping.symbol_name(s).to_string(), label(info), span)
                    })
                    .collect()
            }
            Scoping::Native(model) => model
                .symbol_ids()
                .map(|s| {
                    let info = model.binding_info(s);
                    let span = info.span;
                    (model.symbol_name(s).to_string(), label(info), span)
                })
                .collect(),
        }
    }

    #[cfg(test)]
    pub(crate) fn any_symbol_named(&self, name: &str) -> Option<SymbolId> {
        match &self.scoping {
            Scoping::Oxc(semantic) => {
                let scoping = semantic.scoping();
                scoping
                    .symbol_ids()
                    .find(|s| scoping.symbol_name(*s) == name)
            }
            Scoping::Native(model) => model.symbol_ids().find(|s| model.symbol_name(*s) == name),
        }
    }

    pub fn record_treeshake_import(&mut self, specifier: &str, decl_start: u32, source_span: Span) {
        if self.options.treeshake_compensation
            && !self
                .treeshake_imports
                .iter()
                .any(|t| t.specifier == specifier)
        {
            self.treeshake_imports.push(TreeshakeImport {
                specifier: specifier.to_string(),
                decl_start,
                source_span,
            });
        }
    }

    /// Records a keyframes/positionTry injectable; a repeated name overwrites
    /// in place (upstream keys them by name in one object).
    pub fn record_other_injected_rule(&mut self, rule: StylexRule) {
        if let Some(existing) = self
            .other_injected_rules
            .iter_mut()
            .find(|r| r.class_name == rule.class_name)
        {
            *existing = rule;
        } else {
            self.other_injected_rules.push(rule);
        }
    }
}

fn ref_parent_of<'a>(nodes: &AstNodes<'a>, node: NodeId) -> RefParent<'a> {
    let (_, parent) = climb_parens(nodes, node);
    match member_object_span(nodes.kind(parent)) {
        Some((_, property)) => RefParent::Member { property },
        None => RefParent::Other,
    }
}

// babel's Statement alias: oxc's list plus declaration statements it omits
// (fn/class declarations, imports, named/all exports, TS decl statements).
pub(crate) fn babel_is_statement(kind: AstKind<'_>) -> bool {
    kind.is_statement()
        || matches!(kind, AstKind::Function(f) if f.is_declaration())
        || matches!(kind, AstKind::Class(c) if c.is_declaration())
        || matches!(
            kind,
            AstKind::ImportDeclaration(_)
                | AstKind::ExportDeclaration(_)
                | AstKind::ExportNamedDeclaration(_)
                | AstKind::ExportAllDeclaration(_)
                | AstKind::TSExportAssignment(_)
                | AstKind::TSImportEqualsDeclaration(_)
                | AstKind::TSEnumDeclaration(_)
                | AstKind::TSNamespaceDeclaration(_)
                | AstKind::TSInterfaceDeclaration(_)
                | AstKind::TSTypeAliasDeclaration(_)
        )
}

pub(crate) fn babel_is_export_declaration(kind: AstKind<'_>) -> bool {
    matches!(
        kind,
        AstKind::ExportDeclaration(_)
            | AstKind::ExportNamedDeclaration(_)
            | AstKind::ExportDefaultDeclaration(_)
            | AstKind::ExportAllDeclaration(_)
    )
}

pub(crate) fn binding_pattern_type_name(pattern: &BindingPattern<'_>) -> &'static str {
    match pattern {
        BindingPattern::BindingIdentifier(_) => "Identifier",
        BindingPattern::ObjectPattern(_) => "ObjectPattern",
        BindingPattern::ArrayPattern(_) => "ArrayPattern",
        BindingPattern::AssignmentPattern(_) => "AssignmentPattern",
    }
}

// Capacity hints only (release builds never check them), so oxc skips its
// counting walk; web-corpus p90 ratios, so a rare larger file regrows once.
fn estimated_stats(source_len: usize) -> Stats {
    let per_kb = |n: u64| {
        u32::try_from((source_len as u64 * n) / 1024)
            .unwrap_or(u32::MAX / 2)
            .max(64)
    };
    Stats::new(per_kb(150), per_kb(4), per_kb(10), per_kb(20))
}

// parity: babel constantViolations (write references, redeclarations, var
// for-in/of heads) + evaluate-path.js isMutated, for one symbol.
fn non_constant_flags(semantic: &Semantic<'_>, symbol: SymbolId) -> (bool, bool) {
    let scoping = semantic.scoping();
    let nodes = semantic.nodes();
    let mut violated = !scoping.symbol_redeclarations(symbol).is_empty()
        || is_var_for_head(nodes, scoping.symbol_declaration(symbol));
    let mut mutated = false;
    for reference in scoping.get_resolved_references(symbol) {
        if reference.is_write() {
            violated = true;
        } else if !mutated && reference_is_mutation(nodes, reference.node_id()) {
            mutated = true;
        }
        if violated && mutated {
            break;
        }
    }
    (violated, mutated)
}

fn is_var_for_head(nodes: &AstNodes<'_>, declaration: NodeId) -> bool {
    if !matches!(nodes.kind(declaration), AstKind::VariableDeclarator(_)) {
        return false;
    }
    let decl_id = nodes.parent_id(declaration);
    let AstKind::VariableDeclaration(decl) = nodes.kind(decl_id) else {
        return false;
    };
    if decl.kind != VariableDeclarationKind::Var {
        return false;
    }
    let left = match nodes.kind(nodes.parent_id(decl_id)) {
        AstKind::ForInStatement(stmt) => &stmt.left,
        AstKind::ForOfStatement(stmt) => &stmt.left,
        _ => return false,
    };
    matches!(left, ForStatementLeft::VariableDeclaration(head) if std::ptr::eq(&**head, decl))
}

pub(crate) const MUTATING_ARRAY_METHODS: [&str; 9] = [
    "push",
    "pop",
    "shift",
    "unshift",
    "splice",
    "sort",
    "reverse",
    "fill",
    "copyWithin",
];

const MUTATING_OBJECT_STATICS: [&str; 4] = [
    "assign",
    "defineProperty",
    "defineProperties",
    "setPrototypeOf",
];

/// Climbs from `node` through ParenthesizedExpression parents (babel has no
/// paren nodes, so they are transparent to its parent checks).
fn climb_parens(nodes: &AstNodes<'_>, node: NodeId) -> (NodeId, NodeId) {
    let mut child = node;
    let mut parent = nodes.parent_id(child);
    while child != parent && matches!(nodes.kind(parent), AstKind::ParenthesizedExpression(_)) {
        child = parent;
        parent = nodes.parent_id(child);
    }
    (child, parent)
}

pub(crate) fn member_object_span(kind: AstKind<'_>) -> Option<(Span, Option<&str>)> {
    match kind {
        AstKind::StaticMemberExpression(member) => {
            Some((member.object.span(), Some(member.property.name.as_str())))
        }
        AstKind::ComputedMemberExpression(member) => {
            let property = match &member.expression {
                Expression::Identifier(id) => Some(id.name.as_str()),
                _ => None,
            };
            Some((member.object.span(), property))
        }
        _ => None,
    }
}

fn reference_is_mutation(nodes: &AstNodes<'_>, ref_node: NodeId) -> bool {
    let ref_span = nodes.kind(ref_node).span();
    let (child, parent) = climb_parens(nodes, ref_node);
    if child == parent {
        return false;
    }
    let child_span = nodes.kind(child).span();
    if let Some((object_span, property)) = member_object_span(nodes.kind(parent)) {
        if object_span != child_span {
            return false;
        }
        let _ = ref_span;
        let (member, grandparent) = climb_parens(nodes, parent);
        if member == grandparent {
            return false;
        }
        let member_span = nodes.kind(member).span();
        return match nodes.kind(grandparent) {
            AstKind::AssignmentExpression(assignment) => assignment.left.span() == member_span,
            AstKind::UpdateExpression(_) => true,
            AstKind::UnaryExpression(unary) => {
                unary.operator == oxc_syntax::operator::UnaryOperator::Delete
            }
            AstKind::CallExpression(call) => {
                call.callee.span() == member_span
                    && property.is_some_and(|p| MUTATING_ARRAY_METHODS.contains(&p))
            }
            _ => false,
        };
    }
    if let AstKind::CallExpression(call) = nodes.kind(parent) {
        let first_arg_span = call
            .arguments
            .first()
            .and_then(|a| a.as_expression())
            .map(GetSpan::span);
        return first_arg_span == Some(child_span) && callee_is_object_mutator(&call.callee);
    }
    false
}

// parity: babel matchesPattern('Object.assign') & co — name-only, no bindings.
pub(crate) fn callee_is_object_mutator(callee: &Expression<'_>) -> bool {
    let mut expr = callee;
    while let Expression::ParenthesizedExpression(paren) = expr {
        expr = &paren.expression;
    }
    let Some(member) = expr.as_member_expression() else {
        return false;
    };
    let mut object = member.object();
    while let Expression::ParenthesizedExpression(paren) = object {
        object = &paren.expression;
    }
    let Expression::Identifier(id) = object else {
        return false;
    };
    if id.name != "Object" {
        return false;
    }
    let property = match member {
        oxc_ast::ast::MemberExpression::StaticMemberExpression(m) => Some(m.property.name.as_str()),
        oxc_ast::ast::MemberExpression::ComputedMemberExpression(m) => match &m.expression {
            Expression::StringLiteral(lit) => Some(lit.value.as_str()),
            _ => None,
        },
        oxc_ast::ast::MemberExpression::PrivateFieldExpression(_) => None,
    };
    property.is_some_and(|p| MUTATING_OBJECT_STATICS.contains(&p))
}

/// Per-file newline index: line-start offsets past each `\n`/`\r`/`\r\n`
/// terminator, built in one memchr2 pass and shared by every lookup.
pub struct LineIndex {
    starts: Vec<u32>,
}

impl LineIndex {
    pub fn build(source: &str) -> Self {
        let bytes = source.as_bytes();
        let mut starts: Vec<u32> = Vec::new();
        let mut iter = memchr::memchr2_iter(b'\n', b'\r', bytes).peekable();
        while let Some(i) = iter.next() {
            if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                if iter.peek() == Some(&(i + 1)) {
                    iter.next();
                }
                starts.push((i + 2) as u32);
            } else {
                starts.push((i + 1) as u32);
            }
        }
        LineIndex { starts }
    }

    pub fn line_of(&self, offset: u32) -> u32 {
        1 + self.starts.partition_point(|&start| start <= offset) as u32
    }
}

/// 1-based line of a byte offset, counting `\n`/`\r`/`\r\n` terminators the way
/// Babel `loc.start.line` does.
pub fn line_of_offset(source: &str, offset: u32) -> u32 {
    let head = &source.as_bytes()[..(offset as usize).min(source.len())];
    let mut line = 1;
    let mut i = 0;
    while i < head.len() {
        match head[i] {
            b'\n' => line += 1,
            b'\r' => {
                line += 1;
                if head.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    /// Runs `f` once per scope backend: every assertion pins both.
    fn with_state<R>(source: &str, f: impl Fn(&CompileState<'_>) -> R) -> R {
        let mut out = None;
        for backend in [ScopeBackend::Oxc, ScopeBackend::Native] {
            let allocator = Allocator::default();
            let ret = Parser::new(&allocator, source, SourceType::tsx()).parse();
            assert!(!ret.fatal_error);
            let options = ResolvedOptions::default();
            let imports = scan_imports(&ret.program, &options).unwrap();
            let state = CompileState::build_with_imports_using(
                &ret.program,
                &options,
                None,
                String::new(),
                imports,
                backend,
            );
            out = Some(f(&state));
        }
        out.unwrap()
    }

    fn root_symbol(state: &CompileState<'_>, name: &str) -> SymbolId {
        state
            .root_binding(name)
            .unwrap_or_else(|| panic!("no root binding {name} ({:?})", state.backend()))
    }

    #[test]
    fn module_bindings_resolve_to_declarators() {
        with_state(
            "const a = 1;\nexport const b = 'x';\nlet c;\nfunction f() { const inner = 2; }\n",
            |state| {
                for name in ["a", "b", "c"] {
                    let info = state.binding_info(root_symbol(state, name));
                    assert!(matches!(info.decl, BindingDecl::Declarator(_)), "{name}");
                }
                let f = state.binding_info(root_symbol(state, "f"));
                assert!(matches!(f.decl, BindingDecl::Opaque("FunctionDeclaration")));
                assert!(state.root_binding("inner").is_none());
            },
        );
    }

    #[test]
    fn mutation_scan_matches_upstream_shallow_member_rule() {
        with_state(
            "let a = 1; a = 2;\nconst b = {}; b.x = 1;\nconst c = {}; c.x.y = 1;\n\
             const d = []; d.push(1);\nconst e = {}; Object.assign(e, {});\n\
             const g = {}; delete g.x;\nconst h = 0; h2++;\nvar h2 = 1;\n",
            |state| {
                let non_constant = |name: &str| {
                    let symbol = root_symbol(state, name);
                    state.is_non_constant(symbol) || state.is_mutated(symbol)
                };
                assert!(non_constant("a"));
                assert!(non_constant("b"));
                // Deep member writes do not flag the root, mirroring upstream.
                assert!(!non_constant("c"));
                assert!(non_constant("d"));
                assert!(non_constant("e"));
                assert!(non_constant("g"));
                assert!(non_constant("h2"));
                assert!(!non_constant("h"));
            },
        );
    }

    #[test]
    fn mutation_scan_is_symbol_keyed_and_paren_transparent() {
        with_state(
            "const t = {}; function f() { let t = {}; t.x = 1; }\n\
             const u = {}; (u).x = 1;\n\
             const v = {}; Object.assign((v), {});\n\
             const w = {}; Object.assign((w as any), {});\n\
             const q = []; (q)[\"x\"] = 1;\n",
            |state| {
                let non_constant = |name: &str| {
                    let symbol = root_symbol(state, name);
                    state.is_non_constant(symbol) || state.is_mutated(symbol)
                };
                // Same-named local mutation must not poison the module const.
                assert!(!non_constant("t"));
                assert!(non_constant("u"));
                assert!(non_constant("v"));
                // babel: the TSAsExpression wrapper hides the mutation.
                assert!(!non_constant("w"));
                assert!(non_constant("q"));
            },
        );
    }

    #[test]
    fn for_heads_and_redeclarations_are_violations() {
        with_state(
            "for (var k in {}) {}\nfor (const c of []) {}\nvar d = 1;\nvar d = 2;\n",
            |state| {
                let c = state.any_symbol_named("c").expect("for-of binding");
                assert!(state.is_non_constant(root_symbol(state, "k")));
                assert!(!state.is_non_constant(c));
                assert!(state.is_non_constant(root_symbol(state, "d")));
            },
        );
    }

    #[test]
    fn for_head_rule_is_the_loop_left_only() {
        with_state(
            "for (var k in {}) var body = 1;\nfor (var [a, { b }] of []) {}\n\
             var early; for (var early in {}) {}\nfor (let l of []) {}\n",
            |state| {
                assert!(state.is_non_constant(root_symbol(state, "k")));
                assert!(!state.is_non_constant(root_symbol(state, "body")));
                assert!(state.is_non_constant(root_symbol(state, "a")));
                assert!(state.is_non_constant(root_symbol(state, "b")));
                assert!(state.is_non_constant(root_symbol(state, "early")));
                let l = state.any_symbol_named("l").expect("for-of let binding");
                assert!(!state.is_non_constant(l));
            },
        );
    }

    #[test]
    fn uid_taken_covers_all_declarations_and_references() {
        // codex r4 m14: babel counts unreferenced declarations everywhere,
        // TS type-only ones included (oracle-probed 2026-08-28).
        with_state(
            "const taken = 1;\nfunction f(param) { used; const unused = 1; }\n\
             try {} catch (caught) {}\ntype T = number;\ninterface I { x: number }\n",
            |state| {
                for name in ["taken", "used", "unused", "param", "caught", "T", "I"] {
                    assert!(state.uid_name_taken(name), "{name}");
                }
                assert!(!state.uid_name_taken("_stylex"));
            },
        );
    }

    #[test]
    fn treeshake_and_injected_rule_recording() {
        let options = crate::options::CompilerOptions::from_json(
            &serde_json::json!({ "treeshakeCompensation": true }),
        )
        .unwrap()
        .resolve()
        .unwrap();
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, "", SourceType::tsx()).parse();
        let mut state = CompileState::build(&ret.program, &options, None, String::new()).unwrap();
        state.record_treeshake_import("./a.stylex", 0, Span::new(7, 19));
        state.record_treeshake_import("./b.stylex", 20, Span::new(27, 39));
        state.record_treeshake_import("./a.stylex", 40, Span::new(47, 59));
        let specs: Vec<&str> = state
            .treeshake_imports
            .iter()
            .map(|t| t.specifier.as_str())
            .collect();
        assert_eq!(specs, vec!["./a.stylex", "./b.stylex"]);
        // First evaluation wins the insertion site.
        assert_eq!(state.treeshake_imports[0].decl_start, 0);
    }

    #[test]
    fn line_counting() {
        assert_eq!(line_of_offset("a\nb\nc", 0), 1);
        assert_eq!(line_of_offset("a\nb\nc", 2), 2);
        assert_eq!(line_of_offset("a\nb\nc", 4), 3);
        assert_eq!(line_of_offset("a\r\nb", 3), 2);
        assert_eq!(line_of_offset("a\rb", 2), 2);
        let index = LineIndex::build("a\nb\nc\r\nd\re");
        for offset in 0..=9u32 {
            // The \n of a \r\n diverges (bare-\r count vs same-line); no AST
            // span start can land on that byte, so it is unobservable.
            if offset == 6 {
                continue;
            }
            assert_eq!(
                index.line_of(offset),
                line_of_offset("a\nb\nc\r\nd\re", offset),
                "offset {offset}"
            );
        }
    }
}
