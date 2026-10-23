//! Native binding analysis (`OJ_STYLEX_NATIVE_SCOPE=1`): CompileState's queries from one Visit walk.
// parity: oxc_semantic 0.154 SemanticBuilder + Binder (declaration, hoisting, flags, resolution).

use std::cell::Cell;

use oxc_ast::AstKind;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_span::{GetSpan, Span};
use oxc_str::{Ident, IdentBuildHasher, IdentHashMap};
use oxc_syntax::node::NodeId;
use oxc_syntax::operator::UnaryOperator;
use oxc_syntax::reference::{ReferenceFlags, ReferenceId};
use oxc_syntax::scope::{ScopeFlags, ScopeId};
use oxc_syntax::symbol::{SymbolFlags, SymbolId};

use crate::state::{
    BindingDecl, BindingInfo, MUTATING_ARRAY_METHODS, RefParent, RefSite,
    babel_is_export_declaration, babel_is_statement, binding_pattern_type_name,
    callee_is_object_mutator, member_object_span,
};

/// Which scope analysis backs a [`crate::state::CompileState`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeBackend {
    Oxc,
    Native,
}

impl ScopeBackend {
    /// `OJ_STYLEX_NATIVE_SCOPE=1` selects the native walk; anything else keeps oxc.
    pub fn from_env() -> Self {
        if oj_env::get().knobs.stylex_native_scope {
            ScopeBackend::Native
        } else {
            ScopeBackend::Oxc
        }
    }
}

type Idx = u32;

const ROOT: Idx = 0;

enum DeclSite<'a> {
    Declarator(&'a VariableDeclarator<'a>),
    NamedImport,
    DefaultImport,
    NamespaceImport,
    Param(&'static str),
    Catch,
    Function { expression: bool },
    Class { expression: bool },
    Enum,
    Other,
}

struct Symbol<'a> {
    name: Ident<'a>,
    flags: SymbolFlags,
    /// The declaring node's span (babel `binding.path.node`), not the identifier's.
    span: Span,
    decl: DeclSite<'a>,
    redeclared: bool,
    var_for_head: bool,
    violated: bool,
    mutated: bool,
}

struct Reference<'a> {
    name: Ident<'a>,
    start: u32,
    scope: Idx,
    flags: ReferenceFlags,
    symbol: Option<Idx>,
    parent: RefParent<'a>,
    mutation: bool,
}

struct Scope<'a> {
    parent: Option<Idx>,
    flags: ScopeFlags,
    bindings: Vec<(Ident<'a>, Idx)>,
    /// oxc `hoisting_variables`: `var` names that passed through this scope.
    hoisted: Vec<(Ident<'a>, Idx)>,
    /// Union of [`name_bit`] over `bindings`: a clear bit skips the scan.
    mask: u64,
}

/// `Ident::hash` writes its precomputed word once; this hasher just keeps it.
struct HashWord(u64);

impl std::hash::Hasher for HashWord {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, _: &[u8]) {}

    fn write_u64(&mut self, word: u64) {
        self.0 = word;
    }
}

fn name_bit(name: Ident<'_>) -> u64 {
    let mut word = HashWord(0);
    std::hash::Hash::hash(&name, &mut word);
    1u64 << (word.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58)
}

struct Site {
    scope: Idx,
    top_stmt_start: u32,
    program_level: bool,
}

pub struct ScopeModel<'a> {
    scopes: Vec<Scope<'a>>,
    root_bindings: IdentHashMap<'a, Idx>,
    symbols: Vec<Symbol<'a>>,
    refs: Vec<Reference<'a>>,
    /// Indexed by the `NodeId` written into call/member/JSX-opening cells.
    sites: Vec<Site>,
    root_unresolved: Vec<Idx>,
}

impl<'a> ScopeModel<'a> {
    pub fn build(program: &'a Program<'a>) -> Self {
        let mut builder = Builder::new(program.source_text.len());
        builder.visit_program(program);
        builder.model
    }

    fn get_binding(&self, scope: Idx, name: Ident<'_>) -> Option<Idx> {
        self.get_binding_masked(scope, name, name_bit(name))
    }

    fn get_binding_masked(&self, scope: Idx, name: Ident<'_>, bit: u64) -> Option<Idx> {
        if scope == ROOT {
            return self.root_bindings.get(&name).copied();
        }
        let s = &self.scopes[scope as usize];
        if s.mask & bit == 0 {
            return None;
        }
        s.bindings.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
    }

    fn find_binding(&self, mut scope: Idx, name: Ident<'_>) -> Option<Idx> {
        let bit = name_bit(name);
        loop {
            if let Some(symbol) = self.get_binding_masked(scope, name, bit) {
                return Some(symbol);
            }
            scope = self.scopes[scope as usize].parent?;
        }
    }

    fn site(&self, node: NodeId) -> &Site {
        debug_assert!(node.index() != 0, "untagged node queried");
        &self.sites[node.index()]
    }

    pub fn symbol_of(&self, id: &IdentifierReference<'a>) -> Option<SymbolId> {
        let reference = id.reference_id.get()?;
        self.refs
            .get(reference.index())?
            .symbol
            .map(|s| SymbolId::from_usize(s as usize))
    }

    /// (constantViolations non-empty, isMutated) for one symbol.
    pub fn constness(&self, symbol: SymbolId) -> (bool, bool) {
        let s = &self.symbols[symbol.index()];
        (s.redeclared || s.var_for_head || s.violated, s.mutated)
    }

    pub fn binding_info(&self, symbol: SymbolId) -> BindingInfo<'a> {
        let s = &self.symbols[symbol.index()];
        let decl = match s.decl {
            DeclSite::Declarator(declarator) => BindingDecl::Declarator(declarator),
            DeclSite::NamedImport => BindingDecl::NamedImport,
            DeclSite::DefaultImport => BindingDecl::DefaultImport,
            DeclSite::NamespaceImport => BindingDecl::NamespaceImport,
            DeclSite::Param(type_name) => BindingDecl::Opaque(type_name),
            DeclSite::Catch => BindingDecl::Opaque("CatchClause"),
            DeclSite::Function { expression } => BindingDecl::Opaque(if expression {
                "FunctionExpression"
            } else {
                "FunctionDeclaration"
            }),
            DeclSite::Class { expression } => BindingDecl::Opaque(if expression {
                "ClassExpression"
            } else {
                "ClassDeclaration"
            }),
            DeclSite::Enum => BindingDecl::Opaque("TSEnumDeclaration"),
            DeclSite::Other => BindingDecl::Opaque("Identifier"),
        };
        BindingInfo { decl, span: s.span }
    }

    pub fn root_binding(&self, name: &str) -> Option<SymbolId> {
        self.get_binding(ROOT, Ident::from(name))
            .map(|s| SymbolId::from_usize(s as usize))
    }

    pub fn uid_name_taken(&self, name: &str) -> bool {
        let name = Ident::from(name);
        self.root_unresolved
            .iter()
            .any(|&r| self.refs[r as usize].name == name)
            || self.symbols.iter().any(|s| s.name == name)
    }

    pub fn reference_starts_where(
        &self,
        names: &[&str],
        keep: impl Fn(&str, RefSite<'_, 'a>) -> bool,
    ) -> Vec<u32> {
        let mut starts: Vec<u32> = self
            .refs
            .iter()
            .filter(|r| {
                names.contains(&r.name.as_str()) && keep(r.name.as_str(), RefSite::native(r.parent))
            })
            .map(|r| r.start)
            .collect();
        starts.sort_unstable();
        starts
    }

    pub fn resolves_to_root_binding(&self, node: NodeId, name: &str) -> bool {
        let name = Ident::from(name);
        let Some(root) = self.get_binding(ROOT, name) else {
            return false;
        };
        self.find_binding(self.site(node).scope, name) == Some(root)
    }

    pub fn any_binding_at(&self, node: NodeId, name: &str) -> bool {
        self.find_binding(self.site(node).scope, Ident::from(name))
            .is_some()
    }

    pub fn is_program_level(&self, node: NodeId) -> bool {
        self.site(node).program_level
    }

    pub fn program_statement_start(&self, node: NodeId) -> u32 {
        self.site(node).top_stmt_start
    }

    #[cfg(test)]
    pub(crate) fn symbol_ids(&self) -> impl Iterator<Item = SymbolId> + '_ {
        (0..self.symbols.len()).map(SymbolId::from_usize)
    }

    #[cfg(test)]
    pub(crate) fn symbol_name(&self, symbol: SymbolId) -> &'a str {
        self.symbols[symbol.index()].name.as_str()
    }
}

fn bound_names<'a>(pattern: &'a BindingPattern<'a>, f: &mut impl FnMut(&'a BindingIdentifier<'a>)) {
    match pattern {
        BindingPattern::BindingIdentifier(id) => f(id),
        BindingPattern::ObjectPattern(object) => {
            for property in &object.properties {
                bound_names(&property.value, f);
            }
            if let Some(rest) = &object.rest {
                bound_names(&rest.argument, f);
            }
        }
        BindingPattern::ArrayPattern(array) => {
            for element in array.elements.iter().flatten() {
                bound_names(element, f);
            }
            if let Some(rest) = &array.rest {
                bound_names(&rest.argument, f);
            }
        }
        BindingPattern::AssignmentPattern(assignment) => bound_names(&assignment.left, f),
    }
}

fn upsert<'a>(entries: &mut Vec<(Ident<'a>, Idx)>, name: Ident<'a>, symbol: Idx) {
    match entries.iter_mut().find(|(n, _)| *n == name) {
        Some(entry) => entry.1 = symbol,
        None => entries.push((name, symbol)),
    }
}

struct Builder<'a> {
    model: ScopeModel<'a>,
    current_scope: Idx,
    /// Ancestor kinds, `Program` first; the parent of the node being visited.
    stack: Vec<AstKind<'a>>,
    /// Ancestors that make a node non-program-level (functions, nested statements).
    opaque_depth: u32,
    top_stmt_start: u32,
    reference_flags: ReferenceFlags,
    pending: Vec<Idx>,
    ambient_depth: u32,
    var_decl: Option<(VariableDeclarationKind, bool)>,
    var_head: Option<*const VariableDeclarator<'a>>,
    import_is_type: bool,
    catch_clause_span: Span,
}

impl<'a> Builder<'a> {
    fn new(source_len: usize) -> Self {
        let per_kb = |n: usize| ((source_len * n) / 1024).max(16);
        let mut scopes = Vec::with_capacity(per_kb(6));
        scopes.push(Scope {
            parent: None,
            flags: ScopeFlags::Top,
            bindings: Vec::new(),
            hoisted: Vec::new(),
            mask: 0,
        });
        let mut sites = Vec::with_capacity(per_kb(12));
        sites.push(Site {
            scope: ROOT,
            top_stmt_start: 0,
            program_level: false,
        });
        Builder {
            model: ScopeModel {
                scopes,
                root_bindings: IdentHashMap::with_capacity_and_hasher(per_kb(3), IdentBuildHasher),
                symbols: Vec::with_capacity(per_kb(4)),
                refs: Vec::with_capacity(per_kb(20)),
                sites,
                root_unresolved: Vec::new(),
            },
            current_scope: ROOT,
            stack: Vec::with_capacity(64),
            opaque_depth: 0,
            top_stmt_start: 0,
            reference_flags: ReferenceFlags::empty(),
            pending: Vec::with_capacity(per_kb(20)),
            ambient_depth: 0,
            var_decl: None,
            var_head: None,
            import_is_type: false,
            catch_clause_span: Span::default(),
        }
    }

    fn scope_flags(&self, scope: Idx) -> ScopeFlags {
        self.model.scopes[scope as usize].flags
    }

    fn scope_has_binding(&self, scope: Idx, name: Ident<'a>) -> bool {
        self.model.get_binding(scope, name).is_some()
    }

    fn add_binding(&mut self, scope: Idx, name: Ident<'a>, symbol: Idx) {
        if scope == ROOT {
            self.model.root_bindings.insert(name, symbol);
        } else {
            let s = &mut self.model.scopes[scope as usize];
            s.mask |= name_bit(name);
            upsert(&mut s.bindings, name, symbol);
        }
    }

    fn remove_binding(&mut self, scope: Idx, name: Ident<'a>) {
        if scope == ROOT {
            self.model.root_bindings.remove(&name);
        } else {
            self.model.scopes[scope as usize]
                .bindings
                .retain(|(n, _)| *n != name);
        }
    }

    fn check_redeclaration(&self, scope: Idx, name: Ident<'a>) -> Option<Idx> {
        let symbol = self.model.get_binding(scope, name).or_else(|| {
            self.model.scopes[scope as usize]
                .hoisted
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, s)| *s)
        })?;
        if self.model.symbols[symbol as usize]
            .flags
            .contains(SymbolFlags::FunctionExpression)
        {
            return None;
        }
        Some(symbol)
    }

    fn create_symbol(
        &mut self,
        span: Span,
        name: Ident<'a>,
        flags: SymbolFlags,
        scope: Idx,
        decl: DeclSite<'a>,
    ) -> Idx {
        let var_for_head =
            matches!(decl, DeclSite::Declarator(d) if self.var_head == Some(d as *const _));
        let symbol = self.model.symbols.len() as Idx;
        self.model.symbols.push(Symbol {
            name,
            flags,
            span,
            decl,
            redeclared: false,
            var_for_head,
            violated: false,
            mutated: false,
        });
        self.add_binding(scope, name, symbol);
        symbol
    }

    fn declare_symbol_on_scope(
        &mut self,
        span: Span,
        name: Ident<'a>,
        scope: Idx,
        includes: SymbolFlags,
        decl: DeclSite<'a>,
    ) -> Idx {
        if let Some(symbol) = self.check_redeclaration(scope, name) {
            let s = &mut self.model.symbols[symbol as usize];
            s.redeclared = true;
            s.flags |= includes;
            return symbol;
        }
        self.create_symbol(span, name, includes, scope, decl)
    }

    fn declare_symbol(
        &mut self,
        span: Span,
        name: Ident<'a>,
        includes: SymbolFlags,
        decl: DeclSite<'a>,
    ) -> Idx {
        self.declare_symbol_on_scope(span, name, self.current_scope, includes, decl)
    }

    fn declare_pattern(
        &mut self,
        pattern: &'a BindingPattern<'a>,
        span: Span,
        includes: SymbolFlags,
        decl: impl Fn() -> DeclSite<'a>,
    ) {
        bound_names(pattern, &mut |ident| {
            self.declare_symbol(span, ident.name, includes, decl());
        });
    }

    // parity: Binder for VariableDeclarator — lexical names bind here; `var`
    // hoists to the nearest var scope, merging with any name met on the way.
    fn bind_declarator(&mut self, decl: &'a VariableDeclarator<'a>) {
        let (kind, declare) = self.var_decl.expect("declarator outside a declaration");
        let mut includes = match kind {
            VariableDeclarationKind::Const
            | VariableDeclarationKind::Using
            | VariableDeclarationKind::AwaitUsing => {
                SymbolFlags::BlockScopedVariable | SymbolFlags::ConstVariable
            }
            VariableDeclarationKind::Let => SymbolFlags::BlockScopedVariable,
            VariableDeclarationKind::Var => SymbolFlags::FunctionScopedVariable,
        };
        if declare {
            includes |= SymbolFlags::Ambient;
        }
        if kind.is_lexical() {
            self.declare_pattern(&decl.id, decl.span, includes, || DeclSite::Declarator(decl));
            return;
        }
        let mut target = self.current_scope;
        let mut var_scopes: Vec<Idx> = Vec::new();
        let mut scope = Some(self.current_scope);
        while let Some(s) = scope {
            if self.scope_flags(s).is_var() {
                target = s;
                break;
            }
            var_scopes.push(s);
            scope = self.model.scopes[s as usize].parent;
        }
        bound_names(&decl.id, &mut |ident| {
            let name = ident.name;
            let mut declared = None;
            for &s in &var_scopes {
                if let Some(symbol) = self.check_redeclaration(s, name) {
                    self.model.symbols[symbol as usize].redeclared = true;
                    declared = Some(symbol);
                    if !self.scope_has_binding(target, name) {
                        self.remove_binding(s, name);
                        self.add_binding(target, name, symbol);
                    }
                    break;
                }
            }
            let symbol = declared.unwrap_or_else(|| {
                self.declare_symbol_on_scope(
                    decl.span,
                    name,
                    target,
                    includes,
                    DeclSite::Declarator(decl),
                )
            });
            for &s in &var_scopes {
                upsert(&mut self.model.scopes[s as usize].hoisted, name, symbol);
            }
        });
    }

    fn bind_function(&mut self, func: &'a Function<'a>) {
        let Some(ident) = &func.id else {
            return;
        };
        let mut includes = SymbolFlags::Function;
        if func.declare {
            includes |= SymbolFlags::Ambient;
        }
        if func.is_expression() {
            includes |= SymbolFlags::FunctionExpression;
        }
        if func.r#async || func.generator {
            includes |= SymbolFlags::AsyncOrGeneratorFunction;
        }
        self.declare_symbol(
            func.span,
            ident.name,
            includes,
            DeclSite::Function {
                expression: func.is_expression(),
            },
        );
    }

    fn bind_class(&mut self, class: &'a Class<'a>) {
        let Some(ident) = &class.id else {
            return;
        };
        let mut includes = SymbolFlags::Class;
        if class.declare {
            includes |= SymbolFlags::Ambient;
        }
        self.declare_symbol(
            class.span,
            ident.name,
            includes,
            DeclSite::Class {
                expression: class.is_expression(),
            },
        );
    }

    fn take_reference_flags(&mut self) -> ReferenceFlags {
        if self.reference_flags.is_empty() {
            ReferenceFlags::Read
        } else {
            std::mem::take(&mut self.reference_flags)
        }
    }

    // parity: state.rs reference_is_mutation + the pass-A member check, read
    // off the ancestor stack instead of the node table.
    fn reference_parent(&self, ref_span: Span) -> (RefParent<'a>, bool) {
        let stack = &self.stack;
        let mut i = stack.len();
        let mut child_span = ref_span;
        while i > 0 {
            match stack[i - 1] {
                AstKind::ParenthesizedExpression(paren) => {
                    child_span = paren.span;
                    i -= 1;
                }
                _ => break,
            }
        }
        if i == 0 {
            return (RefParent::Other, false);
        }
        let parent = stack[i - 1];
        if let Some((object_span, property)) = member_object_span(parent) {
            let info = RefParent::Member { property };
            if object_span != child_span {
                return (info, false);
            }
            let mut j = i - 1;
            let mut member_span = parent.span();
            while j > 0 {
                match stack[j - 1] {
                    AstKind::ParenthesizedExpression(paren) => {
                        member_span = paren.span;
                        j -= 1;
                    }
                    _ => break,
                }
            }
            if j == 0 {
                return (info, false);
            }
            let mutation = match stack[j - 1] {
                AstKind::AssignmentExpression(assignment) => assignment.left.span() == member_span,
                AstKind::UpdateExpression(_) => true,
                AstKind::UnaryExpression(unary) => unary.operator == UnaryOperator::Delete,
                AstKind::CallExpression(call) => {
                    call.callee.span() == member_span
                        && property.is_some_and(|p| MUTATING_ARRAY_METHODS.contains(&p))
                }
                _ => false,
            };
            return (info, mutation);
        }
        if let AstKind::CallExpression(call) = parent {
            let first_arg_span = call
                .arguments
                .first()
                .and_then(|a| a.as_expression())
                .map(GetSpan::span);
            let mutation =
                first_arg_span == Some(child_span) && callee_is_object_mutator(&call.callee);
            return (RefParent::Other, mutation);
        }
        (RefParent::Other, false)
    }

    fn push_site(&mut self, cell: &Cell<NodeId>) {
        let index = self.model.sites.len();
        cell.set(NodeId::from_usize(index));
        self.model.sites.push(Site {
            scope: self.current_scope,
            top_stmt_start: self.top_stmt_start,
            program_level: self.opaque_depth == 0,
        });
    }

    // parity: SemanticBuilder try_resolve_reference, flag rewrites included.
    fn try_resolve(&mut self, reference: Idx, symbol: Idx) -> bool {
        let symbol_flags = self.model.symbols[symbol as usize].flags;
        let r = &mut self.model.refs[reference as usize];
        let flags = &mut r.flags;
        let can_resolve = if flags.is_namespace()
            && !flags.is_value_as_type()
            && !symbol_flags.can_be_referenced_as_namespace()
        {
            false
        } else {
            (flags.is_value() && symbol_flags.can_be_referenced_by_value())
                || (flags.is_type() && symbol_flags.can_be_referenced_by_type())
                || (flags.is_value_as_type() && symbol_flags.can_be_referenced_by_value_as_type())
        };
        if !can_resolve {
            return false;
        }
        if (symbol_flags.is_value() && flags.is_value())
            || (flags.is_namespace() && flags.is_read())
        {
            *flags -= ReferenceFlags::Type;
        } else {
            *flags = ReferenceFlags::Type;
        }
        r.symbol = Some(symbol);
        let s = &mut self.model.symbols[symbol as usize];
        if r.flags.is_write() {
            s.violated = true;
        } else if r.mutation {
            s.mutated = true;
        }
        true
    }

    /// Walks up from the reference's lookup scope through `last` (`None` = root).
    fn walk_up_resolve(&mut self, reference: Idx, last: Option<Idx>) -> bool {
        let r = &self.model.refs[reference as usize];
        let name = r.name;
        let bit = name_bit(name);
        let mut scope = Some(r.scope);
        while let Some(s) = scope {
            if let Some(symbol) = self.model.get_binding_masked(s, name, bit)
                && self.try_resolve(reference, symbol)
            {
                return true;
            }
            if Some(s) == last {
                return false;
            }
            scope = self.model.scopes[s as usize].parent;
        }
        false
    }

    fn is_within(&self, mut scope: Idx, ancestor: Idx) -> bool {
        loop {
            if scope == ancestor {
                return true;
            }
            match self.model.scopes[scope as usize].parent {
                Some(parent) => scope = parent,
                None => return false,
            }
        }
    }

    /// Resolves the references recorded since `checkpoint` against the
    /// parameter scope open now. Parameters must not see body declarations, so
    /// a miss resumes from the parent scope at final resolution.
    // parity: SemanticBuilder resolve_references_for_current_scope.
    fn resolve_pending_from(&mut self, checkpoint: usize) {
        let current = self.current_scope;
        let parent = self.model.scopes[current as usize].parent;
        let mut write = checkpoint;
        for read in checkpoint..self.pending.len() {
            let reference = self.pending[read];
            let lookup = self.model.refs[reference as usize].scope;
            // Parameter decorators sit outside this scope chain; leave them.
            if self.is_within(lookup, current) {
                if self.walk_up_resolve(reference, Some(current)) {
                    continue;
                }
                if let Some(parent) = parent {
                    self.model.refs[reference as usize].scope = parent;
                }
            }
            self.pending[write] = reference;
            write += 1;
        }
        self.pending.truncate(write);
    }

    /// Parameter decorators resolve in the enclosing class scope, not the
    /// function's: neither the parameter nor earlier ones are visible.
    // parity: SemanticBuilder visit_parameter_decorators.
    fn visit_parameter_decorators(&mut self, decorators: &oxc_allocator::Vec<'a, Decorator<'a>>) {
        if decorators.is_empty() {
            return;
        }
        let function_scope = self.current_scope;
        self.current_scope = self.model.scopes[function_scope as usize]
            .parent
            .expect("parameter decorators are visited from a function scope");
        self.visit_decorators(decorators);
        self.current_scope = function_scope;
    }

    fn resolve_all(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        for reference in pending {
            if !self.walk_up_resolve(reference, None) {
                self.model.root_unresolved.push(reference);
            }
        }
    }

    fn enter_ambient(&mut self, is_ambient: bool) {
        self.ambient_depth += u32::from(is_ambient);
    }

    fn leave_ambient(&mut self, is_ambient: bool) {
        self.ambient_depth -= u32::from(is_ambient);
    }

    fn in_ambient_context(&self) -> bool {
        self.ambient_depth > 0
    }

    // parity: TSNamespaceDeclaration binder — a namespace holding only types
    // is a NamespaceModule; `export { x }` aliases are read as instantiating.
    fn namespace_is_instantiated(decl: &TSNamespaceDeclaration<'a>) -> bool {
        match &decl.body {
            TSNamespaceDeclarationBody::TSNamespaceDeclaration(inner) => {
                Self::namespace_is_instantiated(inner)
            }
            TSNamespaceDeclarationBody::TSModuleBlock(block) => {
                block.body.iter().any(Self::statement_instantiates)
            }
        }
    }

    fn statement_instantiates(stmt: &Statement<'a>) -> bool {
        match stmt {
            Statement::TSInterfaceDeclaration(_)
            | Statement::TSTypeAliasDeclaration(_)
            | Statement::TSImportEqualsDeclaration(_) => false,
            Statement::ExportDefaultDeclaration(export) => !matches!(
                export.declaration,
                ExportDefaultDeclarationKind::TSInterfaceDeclaration(_)
            ),
            Statement::ExportDeclaration(export) => match &export.declaration {
                Declaration::TSNamespaceDeclaration(inner) => {
                    Self::namespace_is_instantiated(inner)
                }
                decl => !decl.is_type(),
            },
            Statement::TSNamespaceDeclaration(inner) => Self::namespace_is_instantiated(inner),
            _ => true,
        }
    }
}

// parity: ast-helpers.js isProgramLevel, kept as a depth counter: a node counts
// when it is function-like or a statement not hanging off Program/export.
fn opaque_step(kind: AstKind<'_>, parent: Option<AstKind<'_>>) -> bool {
    kind.is_function_like()
        || (babel_is_statement(kind)
            && !parent.is_some_and(|p| {
                matches!(p, AstKind::Program(_)) || babel_is_export_declaration(p)
            }))
}

impl<'a> Visit<'a> for Builder<'a> {
    fn enter_node(&mut self, kind: AstKind<'a>) {
        if self.stack.len() == 1 {
            self.top_stmt_start = kind.span().start;
        }
        if opaque_step(kind, self.stack.last().copied()) {
            self.opaque_depth += 1;
        }
        self.stack.push(kind);
    }

    fn leave_node(&mut self, kind: AstKind<'a>) {
        self.stack.pop();
        if opaque_step(kind, self.stack.last().copied()) {
            self.opaque_depth -= 1;
        }
    }

    fn enter_scope(&mut self, flags: ScopeFlags, _scope_id: &Cell<Option<ScopeId>>) {
        let index = self.model.scopes.len() as Idx;
        self.model.scopes.push(Scope {
            parent: Some(self.current_scope),
            flags,
            bindings: Vec::new(),
            hoisted: Vec::new(),
            mask: 0,
        });
        self.current_scope = index;
    }

    fn leave_scope(&mut self) {
        self.current_scope = self.model.scopes[self.current_scope as usize]
            .parent
            .expect("leave_scope on the root scope");
    }

    fn visit_program(&mut self, program: &Program<'a>) {
        self.stack.push(AstKind::Program(self.alloc(program)));
        self.visit_statements(&program.body);
        self.resolve_all();
        self.stack.pop();
    }

    // Leaves never parent a reference and are bound by their parents: skip
    // their enter/leave bookkeeping (about half of all nodes).
    fn visit_identifier_name(&mut self, _: &IdentifierName<'a>) {}
    fn visit_binding_identifier(&mut self, _: &BindingIdentifier<'a>) {}
    fn visit_label_identifier(&mut self, _: &LabelIdentifier<'a>) {}
    fn visit_private_identifier(&mut self, _: &PrivateIdentifier<'a>) {}
    fn visit_this_expression(&mut self, _: &ThisExpression) {}
    fn visit_super(&mut self, _: &Super) {}
    fn visit_boolean_literal(&mut self, _: &BooleanLiteral) {}
    fn visit_null_literal(&mut self, _: &NullLiteral) {}
    fn visit_numeric_literal(&mut self, _: &NumericLiteral<'a>) {}
    fn visit_string_literal(&mut self, _: &StringLiteral<'a>) {}
    fn visit_big_int_literal(&mut self, _: &BigIntLiteral<'a>) {}
    fn visit_reg_exp_literal(&mut self, _: &RegExpLiteral<'a>) {}
    fn visit_template_element(&mut self, _: &TemplateElement<'a>) {}
    fn visit_jsx_text(&mut self, _: &JSXText<'a>) {}
    fn visit_jsx_identifier(&mut self, _: &JSXIdentifier<'a>) {}
    fn visit_jsx_empty_expression(&mut self, _: &JSXEmptyExpression) {}
    fn visit_empty_statement(&mut self, _: &EmptyStatement) {}
    fn visit_debugger_statement(&mut self, _: &DebuggerStatement) {}
    fn visit_directive(&mut self, _: &Directive<'a>) {}
    fn visit_hashbang(&mut self, _: &Hashbang<'a>) {}
    fn visit_ts_type_annotation(&mut self, it: &TSTypeAnnotation<'a>) {
        if !matches!(
            it.type_annotation,
            TSType::TSAnyKeyword(_)
                | TSType::TSStringKeyword(_)
                | TSType::TSNumberKeyword(_)
                | TSType::TSBooleanKeyword(_)
                | TSType::TSVoidKeyword(_)
                | TSType::TSUndefinedKeyword(_)
                | TSType::TSNullKeyword(_)
                | TSType::TSUnknownKeyword(_)
                | TSType::TSNeverKeyword(_)
        ) {
            walk::walk_ts_type_annotation(self, it);
        }
    }

    fn visit_identifier_reference(&mut self, ident: &IdentifierReference<'a>) {
        let ident = self.alloc(ident);
        let flags = self.take_reference_flags();
        let (parent, mutation) = self.reference_parent(ident.span);
        let index = self.model.refs.len() as Idx;
        self.model.refs.push(Reference {
            name: ident.name,
            start: ident.span.start,
            scope: self.current_scope,
            flags,
            symbol: None,
            parent,
            mutation,
        });
        ident
            .reference_id
            .set(Some(ReferenceId::from_usize(index as usize)));
        self.pending.push(index);
    }

    fn visit_call_expression(&mut self, expr: &CallExpression<'a>) {
        let expr = self.alloc(expr);
        let kind = AstKind::CallExpression(expr);
        self.enter_node(kind);
        self.push_site(&expr.node_id);
        self.visit_expression(&expr.callee);
        if let Some(type_arguments) = &expr.type_arguments {
            self.visit_ts_type_parameter_instantiation(type_arguments);
        }
        self.visit_arguments(&expr.arguments);
        self.leave_node(kind);
    }

    fn visit_jsx_opening_element(&mut self, it: &JSXOpeningElement<'a>) {
        let it = self.alloc(it);
        let kind = AstKind::JSXOpeningElement(it);
        self.enter_node(kind);
        self.push_site(&it.node_id);
        self.visit_jsx_element_name(&it.name);
        if let Some(type_arguments) = &it.type_arguments {
            self.visit_ts_type_parameter_instantiation(type_arguments);
        }
        self.visit_jsx_attribute_items(&it.attributes);
        self.leave_node(kind);
    }

    fn visit_member_expression(&mut self, it: &MemberExpression<'a>) {
        if self.reference_flags.is_write() {
            self.reference_flags = ReferenceFlags::Read | ReferenceFlags::MemberWriteTarget;
        } else {
            self.reference_flags -= ReferenceFlags::Write;
        }
        match it {
            MemberExpression::ComputedMemberExpression(it) => {
                self.visit_computed_member_expression(it);
            }
            MemberExpression::StaticMemberExpression(it) => self.visit_static_member_expression(it),
            MemberExpression::PrivateFieldExpression(it) => self.visit_private_field_expression(it),
        }
        self.reference_flags = ReferenceFlags::empty();
    }

    fn visit_static_member_expression(&mut self, it: &StaticMemberExpression<'a>) {
        let it = self.alloc(it);
        let kind = AstKind::StaticMemberExpression(it);
        self.enter_node(kind);
        self.push_site(&it.node_id);
        self.visit_expression(&it.object);
        self.visit_identifier_name(&it.property);
        self.leave_node(kind);
    }

    fn visit_computed_member_expression(&mut self, it: &ComputedMemberExpression<'a>) {
        let it = self.alloc(it);
        let kind = AstKind::ComputedMemberExpression(it);
        self.enter_node(kind);
        self.push_site(&it.node_id);
        self.visit_expression(&it.object);
        self.reference_flags -= ReferenceFlags::MemberWriteTarget;
        self.visit_expression(&it.expression);
        self.leave_node(kind);
    }

    fn visit_update_expression(&mut self, it: &UpdateExpression<'a>) {
        let kind = AstKind::UpdateExpression(self.alloc(it));
        self.enter_node(kind);
        self.reference_flags = ReferenceFlags::read_write();
        self.visit_simple_assignment_target(&it.argument);
        self.leave_node(kind);
    }

    fn visit_unary_expression(&mut self, it: &UnaryExpression<'a>) {
        let kind = AstKind::UnaryExpression(self.alloc(it));
        self.enter_node(kind);
        if it.operator == UnaryOperator::Delete && it.argument.is_member_expression() {
            self.reference_flags = ReferenceFlags::Write;
        }
        self.visit_expression(&it.argument);
        self.leave_node(kind);
    }

    fn visit_assignment_expression(&mut self, expr: &AssignmentExpression<'a>) {
        let kind = AstKind::AssignmentExpression(self.alloc(expr));
        self.enter_node(kind);
        if !expr.operator.is_assign() {
            self.reference_flags = ReferenceFlags::read_write();
        }
        self.visit_assignment_target(&expr.left);
        self.visit_expression(&expr.right);
        self.leave_node(kind);
    }

    fn visit_conditional_expression(&mut self, expr: &ConditionalExpression<'a>) {
        let kind = AstKind::ConditionalExpression(self.alloc(expr));
        self.enter_node(kind);
        let saved_flags = self.reference_flags;
        self.reference_flags -= ReferenceFlags::MemberWriteTarget;
        self.visit_expression(&expr.test);
        self.reference_flags = saved_flags;
        self.visit_expression(&expr.consequent);
        self.visit_expression(&expr.alternate);
        self.leave_node(kind);
    }

    fn visit_simple_assignment_target(&mut self, it: &SimpleAssignmentTarget<'a>) {
        if !self.reference_flags.is_write() {
            self.reference_flags = ReferenceFlags::Write;
        }
        match it {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(it) => {
                self.visit_identifier_reference(it);
            }
            SimpleAssignmentTarget::TSAsExpression(it) => self.visit_ts_as_expression(it),
            SimpleAssignmentTarget::TSSatisfiesExpression(it) => {
                self.visit_ts_satisfies_expression(it);
            }
            SimpleAssignmentTarget::TSNonNullExpression(it) => {
                self.visit_ts_non_null_expression(it)
            }
            SimpleAssignmentTarget::TSTypeAssertion(it) => self.visit_ts_type_assertion(it),
            _ => self.visit_member_expression(it.to_member_expression()),
        }
    }

    fn visit_assignment_target_property_identifier(
        &mut self,
        it: &AssignmentTargetPropertyIdentifier<'a>,
    ) {
        let kind = AstKind::AssignmentTargetPropertyIdentifier(self.alloc(it));
        self.enter_node(kind);
        self.reference_flags = ReferenceFlags::Write;
        self.visit_identifier_reference(&it.binding);
        if let Some(init) = &it.init {
            self.visit_expression(init);
        }
        self.leave_node(kind);
    }

    fn visit_export_default_declaration_kind(&mut self, it: &ExportDefaultDeclarationKind<'a>) {
        match it {
            ExportDefaultDeclarationKind::FunctionDeclaration(it) => {
                self.visit_function(it, ScopeFlags::Function);
            }
            ExportDefaultDeclarationKind::ClassDeclaration(it) => self.visit_class(it),
            ExportDefaultDeclarationKind::TSInterfaceDeclaration(it) => {
                self.visit_ts_interface_declaration(it);
            }
            ExportDefaultDeclarationKind::Identifier(it) => {
                self.reference_flags = ReferenceFlags::Read | ReferenceFlags::Type;
                self.visit_identifier_reference(it);
            }
            _ => self.visit_expression(it.to_expression()),
        }
    }

    fn visit_export_named_declaration(&mut self, it: &ExportNamedDeclaration<'a>) {
        let kind = AstKind::ExportNamedDeclaration(self.alloc(it));
        self.enter_node(kind);
        for specifier in &it.specifiers {
            self.reference_flags = if it.export_kind.is_type() || specifier.export_kind.is_type() {
                ReferenceFlags::Type
            } else {
                ReferenceFlags::Read | ReferenceFlags::Type
            };
            self.visit_export_specifier(specifier);
        }
        self.leave_node(kind);
    }

    fn visit_ts_export_assignment(&mut self, it: &TSExportAssignment<'a>) {
        let kind = AstKind::TSExportAssignment(self.alloc(it));
        self.enter_node(kind);
        if it.expression.is_identifier_reference() {
            self.reference_flags = ReferenceFlags::Read | ReferenceFlags::Type;
        }
        self.visit_expression(&it.expression);
        self.leave_node(kind);
    }

    fn visit_ts_type_query(&mut self, ty: &TSTypeQuery<'a>) {
        let kind = AstKind::TSTypeQuery(self.alloc(ty));
        self.enter_node(kind);
        self.reference_flags = ReferenceFlags::ValueAsType;
        self.visit_ts_type_query_expr_name(&ty.expr_name);
        if let Some(type_arguments) = &ty.type_arguments {
            self.visit_ts_type_parameter_instantiation(type_arguments);
        }
        self.reference_flags = ReferenceFlags::empty();
        self.leave_node(kind);
    }

    fn visit_ts_property_signature(&mut self, sig: &TSPropertySignature<'a>) {
        let kind = AstKind::TSPropertySignature(self.alloc(sig));
        self.enter_node(kind);
        if sig.key.is_expression() {
            self.reference_flags = ReferenceFlags::ValueAsType;
        }
        self.visit_property_key(&sig.key);
        if let Some(type_annotation) = &sig.type_annotation {
            self.visit_ts_type_annotation(type_annotation);
        }
        self.reference_flags = ReferenceFlags::empty();
        self.leave_node(kind);
    }

    fn visit_ts_method_signature(&mut self, sig: &TSMethodSignature<'a>) {
        let kind = AstKind::TSMethodSignature(self.alloc(sig));
        self.enter_node(kind);
        self.enter_scope(ScopeFlags::empty(), &sig.scope_id);
        if sig.computed {
            self.reference_flags = ReferenceFlags::ValueAsType;
        }
        self.visit_property_key(&sig.key);
        self.reference_flags = ReferenceFlags::empty();
        if let Some(type_parameters) = &sig.type_parameters {
            self.visit_ts_type_parameter_declaration(type_parameters);
        }
        if let Some(this_param) = &sig.this_param {
            self.visit_ts_this_parameter(this_param);
        }
        self.visit_formal_parameters(&sig.params);
        if let Some(return_type) = &sig.return_type {
            self.visit_ts_type_annotation(return_type);
        }
        self.leave_scope();
        self.leave_node(kind);
    }

    fn visit_method_definition(&mut self, method: &MethodDefinition<'a>) {
        let kind = AstKind::MethodDefinition(self.alloc(method));
        self.enter_node(kind);
        self.visit_decorators(&method.decorators);
        if method.computed && (method.r#type.is_abstract() || self.in_ambient_context()) {
            self.reference_flags = ReferenceFlags::ValueAsType;
        }
        self.visit_property_key(&method.key);
        self.reference_flags = ReferenceFlags::empty();
        let flags = match method.kind {
            MethodDefinitionKind::Get => ScopeFlags::Function | ScopeFlags::GetAccessor,
            MethodDefinitionKind::Set => ScopeFlags::Function | ScopeFlags::SetAccessor,
            MethodDefinitionKind::Constructor => ScopeFlags::Function | ScopeFlags::Constructor,
            MethodDefinitionKind::Method => ScopeFlags::Function,
        };
        self.visit_function(&method.value, flags);
        self.leave_node(kind);
    }

    fn visit_property_definition(&mut self, prop: &PropertyDefinition<'a>) {
        let kind = AstKind::PropertyDefinition(self.alloc(prop));
        self.enter_node(kind);
        self.visit_decorators(&prop.decorators);
        self.enter_ambient(prop.declare);
        if prop.computed && (prop.r#type.is_abstract() || self.in_ambient_context()) {
            self.reference_flags = ReferenceFlags::ValueAsType;
        }
        self.visit_property_key(&prop.key);
        self.reference_flags = ReferenceFlags::empty();
        if let Some(type_annotation) = &prop.type_annotation {
            self.visit_ts_type_annotation(type_annotation);
        }
        if let Some(value) = &prop.value {
            self.visit_expression(value);
        }
        self.leave_node(kind);
        self.leave_ambient(prop.declare);
    }

    fn visit_ts_interface_heritage(&mut self, heritage: &TSInterfaceHeritage<'a>) {
        let kind = AstKind::TSInterfaceHeritage(self.alloc(heritage));
        self.enter_node(kind);
        self.reference_flags = ReferenceFlags::Type;
        self.visit_ts_type_name(&heritage.type_name);
        if let Some(type_arguments) = &heritage.type_arguments {
            self.visit_ts_type_parameter_instantiation(type_arguments);
        }
        self.leave_node(kind);
    }

    fn visit_ts_class_implements(&mut self, implements: &TSClassImplements<'a>) {
        let kind = AstKind::TSClassImplements(self.alloc(implements));
        self.enter_node(kind);
        self.reference_flags = ReferenceFlags::Type;
        self.visit_ts_type_name(&implements.expression);
        if let Some(type_arguments) = &implements.type_arguments {
            self.visit_ts_type_parameter_instantiation(type_arguments);
        }
        self.leave_node(kind);
    }

    fn visit_ts_type_reference(&mut self, ty: &TSTypeReference<'a>) {
        let kind = AstKind::TSTypeReference(self.alloc(ty));
        self.enter_node(kind);
        self.reference_flags = ReferenceFlags::Type;
        self.visit_ts_type_name(&ty.type_name);
        if let Some(type_arguments) = &ty.type_arguments {
            self.visit_ts_type_parameter_instantiation(type_arguments);
        }
        self.leave_node(kind);
    }

    fn visit_ts_qualified_name(&mut self, name: &TSQualifiedName<'a>) {
        let kind = AstKind::TSQualifiedName(self.alloc(name));
        self.enter_node(kind);
        if self.reference_flags.is_empty() {
            self.reference_flags =
                ReferenceFlags::Read | ReferenceFlags::Type | ReferenceFlags::Namespace;
        } else {
            self.reference_flags |= ReferenceFlags::Namespace;
        }
        self.visit_ts_type_name(&name.left);
        self.visit_identifier_name(&name.right);
        self.leave_node(kind);
    }

    fn visit_class(&mut self, class: &Class<'a>) {
        let class = self.alloc(class);
        let kind = AstKind::Class(class);
        self.enter_node(kind);
        if class.is_declaration() {
            self.bind_class(class);
        }
        self.visit_decorators(&class.decorators);
        self.enter_ambient(class.declare);
        self.enter_scope(ScopeFlags::StrictMode, &class.scope_id);
        if class.is_expression() {
            self.bind_class(class);
        }
        if let Some(type_parameters) = &class.type_parameters {
            self.visit_ts_type_parameter_declaration(type_parameters);
        }
        if let Some(heritage) = &class.heritage {
            if self.in_ambient_context() {
                self.reference_flags = ReferenceFlags::ValueAsType;
            }
            self.visit_expression(&heritage.expression);
            self.reference_flags = ReferenceFlags::empty();
            if let Some(type_arguments) = &heritage.type_arguments {
                self.visit_ts_type_parameter_instantiation(type_arguments);
            }
        }
        self.visit_ts_class_implements_list(&class.implements);
        self.visit_class_body(&class.body);
        self.leave_scope();
        self.leave_node(kind);
        self.leave_ambient(class.declare);
    }

    fn visit_function(&mut self, func: &Function<'a>, flags: ScopeFlags) {
        let func = self.alloc(func);
        let kind = AstKind::Function(func);
        self.enter_node(kind);
        self.enter_ambient(func.declare);
        if func.is_declaration() {
            self.bind_function(func);
        }
        self.enter_scope(flags, &func.scope_id);
        if func.is_expression() {
            self.bind_function(func);
        }
        let checkpoint = self.pending.len();
        if let Some(type_parameters) = &func.type_parameters {
            self.visit_ts_type_parameter_declaration(type_parameters);
        }
        if let Some(this_param) = &func.this_param {
            self.visit_ts_this_parameter(this_param);
        }
        self.visit_formal_parameters(&func.params);
        if let Some(return_type) = &func.return_type {
            self.visit_ts_type_annotation(return_type);
        }
        self.resolve_pending_from(checkpoint);
        if let Some(body) = &func.body {
            self.visit_function_body(body);
        }
        self.leave_scope();
        self.leave_node(kind);
        self.leave_ambient(func.declare);
    }

    fn visit_arrow_function_expression(&mut self, expr: &ArrowFunctionExpression<'a>) {
        let kind = AstKind::ArrowFunctionExpression(self.alloc(expr));
        self.enter_node(kind);
        self.enter_scope(ScopeFlags::Function | ScopeFlags::Arrow, &expr.scope_id);
        let checkpoint = self.pending.len();
        if let Some(parameters) = &expr.type_parameters {
            self.visit_ts_type_parameter_declaration(parameters);
        }
        self.visit_formal_parameters(&expr.params);
        if let Some(return_type) = &expr.return_type {
            self.visit_ts_type_annotation(return_type);
        }
        self.resolve_pending_from(checkpoint);
        self.visit_arrow_function_body(&expr.body);
        self.leave_scope();
        self.leave_node(kind);
    }

    fn visit_formal_parameter(&mut self, param: &FormalParameter<'a>) {
        let param = self.alloc(param);
        let kind = AstKind::FormalParameter(param);
        self.enter_node(kind);
        let type_name = binding_pattern_type_name(&param.pattern);
        self.declare_pattern(
            &param.pattern,
            param.span,
            SymbolFlags::FunctionScopedVariable,
            || DeclSite::Param(type_name),
        );
        self.visit_parameter_decorators(&param.decorators);
        self.visit_binding_pattern(&param.pattern);
        if let Some(type_annotation) = &param.type_annotation {
            self.visit_ts_type_annotation(type_annotation);
        }
        if let Some(initializer) = &param.initializer {
            self.visit_expression(initializer);
        }
        self.leave_node(kind);
    }

    fn visit_formal_parameter_rest(&mut self, param: &FormalParameterRest<'a>) {
        let param = self.alloc(param);
        let kind = AstKind::FormalParameterRest(param);
        self.enter_node(kind);
        self.declare_pattern(
            &param.rest.argument,
            param.span,
            SymbolFlags::FunctionScopedVariable,
            || DeclSite::Other,
        );
        self.visit_parameter_decorators(&param.decorators);
        self.visit_binding_rest_element(&param.rest);
        if let Some(type_annotation) = &param.type_annotation {
            self.visit_ts_type_annotation(type_annotation);
        }
        self.leave_node(kind);
    }

    fn visit_catch_clause(&mut self, clause: &CatchClause<'a>) {
        self.catch_clause_span = clause.span;
        walk::walk_catch_clause(self, clause);
    }

    fn visit_catch_parameter(&mut self, param: &CatchParameter<'a>) {
        let param = self.alloc(param);
        let kind = AstKind::CatchParameter(param);
        self.enter_node(kind);
        let clause_span = self.catch_clause_span;
        if let BindingPattern::BindingIdentifier(ident) = &param.pattern {
            self.create_symbol(
                clause_span,
                ident.name,
                SymbolFlags::FunctionScopedVariable | SymbolFlags::CatchVariable,
                self.current_scope,
                DeclSite::Catch,
            );
        } else {
            self.declare_pattern(
                &param.pattern,
                clause_span,
                SymbolFlags::BlockScopedVariable | SymbolFlags::CatchVariable,
                || DeclSite::Catch,
            );
        }
        let checkpoint = self.pending.len();
        self.visit_binding_pattern(&param.pattern);
        if let Some(type_annotation) = &param.type_annotation {
            self.visit_ts_type_annotation(type_annotation);
        }
        self.resolve_pending_from(checkpoint);
        self.leave_node(kind);
    }

    fn visit_block_statement(&mut self, it: &BlockStatement<'a>) {
        let kind = AstKind::BlockStatement(self.alloc(it));
        self.enter_node(kind);
        let parent_scope = self.current_scope;
        self.enter_scope(ScopeFlags::empty(), &it.scope_id);
        if self.scope_flags(parent_scope).is_catch_clause() {
            let parent = &mut self.model.scopes[parent_scope as usize];
            let (moved, mask) = (std::mem::take(&mut parent.bindings), parent.mask);
            let block = &mut self.model.scopes[self.current_scope as usize];
            block.bindings = moved;
            block.mask |= mask;
        }
        self.visit_statements(&it.body);
        self.leave_scope();
        self.leave_node(kind);
    }

    fn visit_for_in_statement(&mut self, stmt: &ForInStatement<'a>) {
        let kind = AstKind::ForInStatement(self.alloc(stmt));
        self.enter_node(kind);
        self.enter_scope(ScopeFlags::empty(), &stmt.scope_id);
        self.var_head = var_head_declarator(&stmt.left);
        self.visit_for_statement_left(&stmt.left);
        self.var_head = None;
        self.visit_expression(&stmt.right);
        self.visit_statement(&stmt.body);
        self.leave_scope();
        self.leave_node(kind);
    }

    fn visit_for_of_statement(&mut self, stmt: &ForOfStatement<'a>) {
        let kind = AstKind::ForOfStatement(self.alloc(stmt));
        self.enter_node(kind);
        self.enter_scope(ScopeFlags::empty(), &stmt.scope_id);
        self.var_head = var_head_declarator(&stmt.left);
        self.visit_for_statement_left(&stmt.left);
        self.var_head = None;
        self.visit_expression(&stmt.right);
        self.visit_statement(&stmt.body);
        self.leave_scope();
        self.leave_node(kind);
    }

    fn visit_variable_declaration(&mut self, it: &VariableDeclaration<'a>) {
        let kind = AstKind::VariableDeclaration(self.alloc(it));
        self.enter_node(kind);
        self.enter_ambient(it.declare);
        let saved = self.var_decl.replace((it.kind, it.declare));
        self.visit_variable_declarators(&it.declarations);
        self.var_decl = saved;
        self.leave_node(kind);
        self.leave_ambient(it.declare);
    }

    fn visit_variable_declarator(&mut self, decl: &VariableDeclarator<'a>) {
        let decl = self.alloc(decl);
        let kind = AstKind::VariableDeclarator(decl);
        self.enter_node(kind);
        self.bind_declarator(decl);
        self.visit_binding_pattern(&decl.id);
        if let Some(type_annotation) = &decl.type_annotation {
            self.visit_ts_type_annotation(type_annotation);
        }
        if let Some(init) = &decl.init {
            self.visit_expression(init);
        }
        self.leave_node(kind);
    }

    fn visit_import_declaration(&mut self, decl: &ImportDeclaration<'a>) {
        let kind = AstKind::ImportDeclaration(self.alloc(decl));
        self.enter_node(kind);
        self.import_is_type = decl.import_kind.is_type();
        if let Some(specifiers) = &decl.specifiers {
            for specifier in specifiers {
                self.visit_import_declaration_specifier(specifier);
            }
        }
        self.leave_node(kind);
    }

    fn visit_import_specifier(&mut self, specifier: &ImportSpecifier<'a>) {
        let specifier = self.alloc(specifier);
        let kind = AstKind::ImportSpecifier(specifier);
        self.enter_node(kind);
        let includes = if specifier.import_kind.is_type() || self.import_is_type {
            SymbolFlags::TypeImport
        } else {
            SymbolFlags::Import
        };
        self.declare_symbol(
            specifier.span,
            specifier.local.name,
            includes,
            DeclSite::NamedImport,
        );
        self.leave_node(kind);
    }

    fn visit_import_default_specifier(&mut self, specifier: &ImportDefaultSpecifier<'a>) {
        let specifier = self.alloc(specifier);
        let kind = AstKind::ImportDefaultSpecifier(specifier);
        self.enter_node(kind);
        let includes = if self.import_is_type {
            SymbolFlags::TypeImport
        } else {
            SymbolFlags::Import
        };
        self.declare_symbol(
            specifier.span,
            specifier.local.name,
            includes,
            DeclSite::DefaultImport,
        );
        self.leave_node(kind);
    }

    fn visit_import_namespace_specifier(&mut self, specifier: &ImportNamespaceSpecifier<'a>) {
        let specifier = self.alloc(specifier);
        let kind = AstKind::ImportNamespaceSpecifier(specifier);
        self.enter_node(kind);
        let includes = if self.import_is_type {
            SymbolFlags::TypeImport
        } else {
            SymbolFlags::Import
        };
        self.declare_symbol(
            specifier.span,
            specifier.local.name,
            includes,
            DeclSite::NamespaceImport,
        );
        self.leave_node(kind);
    }

    fn visit_ts_import_equals_declaration(&mut self, decl: &TSImportEqualsDeclaration<'a>) {
        let decl = self.alloc(decl);
        let kind = AstKind::TSImportEqualsDeclaration(decl);
        self.enter_node(kind);
        self.declare_symbol(
            decl.span,
            decl.id.name,
            SymbolFlags::Import,
            DeclSite::Other,
        );
        self.reference_flags = if decl.import_kind.is_type() {
            ReferenceFlags::Type
        } else {
            ReferenceFlags::Read | ReferenceFlags::Type
        };
        self.visit_ts_module_reference(&decl.module_reference);
        self.reference_flags = ReferenceFlags::empty();
        self.leave_node(kind);
    }

    fn visit_ts_module_reference(&mut self, module_reference: &TSModuleReference<'a>) {
        match module_reference {
            TSModuleReference::ExternalModuleReference(reference) => {
                self.visit_ts_external_module_reference(reference);
            }
            TSModuleReference::IdentifierReference(reference) => {
                self.reference_flags |= ReferenceFlags::Namespace;
                self.visit_identifier_reference(reference);
            }
            TSModuleReference::QualifiedName(name) => self.visit_ts_qualified_name(name),
        }
    }

    fn visit_ts_external_module_declaration(&mut self, decl: &TSExternalModuleDeclaration<'a>) {
        self.enter_ambient(decl.declare);
        walk::walk_ts_external_module_declaration(self, decl);
        self.leave_ambient(decl.declare);
    }

    fn visit_ts_global_declaration(&mut self, decl: &TSGlobalDeclaration<'a>) {
        self.enter_ambient(decl.declare);
        walk::walk_ts_global_declaration(self, decl);
        self.leave_ambient(decl.declare);
    }

    fn visit_ts_namespace_declaration(&mut self, decl: &TSNamespaceDeclaration<'a>) {
        let decl = self.alloc(decl);
        let kind = AstKind::TSNamespaceDeclaration(decl);
        self.enter_node(kind);
        self.enter_ambient(decl.declare);
        let mut includes = if Self::namespace_is_instantiated(decl) {
            SymbolFlags::ValueModule
        } else {
            SymbolFlags::NamespaceModule
        };
        if decl.declare {
            includes |= SymbolFlags::Ambient;
        }
        self.declare_symbol(decl.span, decl.id.name, includes, DeclSite::Other);
        self.enter_scope(ScopeFlags::TsModuleBlock, &decl.scope_id);
        self.visit_ts_namespace_declaration_body(&decl.body);
        self.leave_scope();
        self.leave_node(kind);
        self.leave_ambient(decl.declare);
    }

    fn visit_ts_type_alias_declaration(&mut self, decl: &TSTypeAliasDeclaration<'a>) {
        let decl = self.alloc(decl);
        let kind = AstKind::TSTypeAliasDeclaration(decl);
        self.enter_node(kind);
        self.enter_ambient(decl.declare);
        let mut includes = SymbolFlags::TypeAlias;
        if decl.declare {
            includes |= SymbolFlags::Ambient;
        }
        self.declare_symbol(decl.span, decl.id.name, includes, DeclSite::Other);
        self.enter_scope(ScopeFlags::empty(), &decl.scope_id);
        if let Some(type_parameters) = &decl.type_parameters {
            self.visit_ts_type_parameter_declaration(type_parameters);
        }
        self.visit_ts_type(&decl.type_annotation);
        self.leave_scope();
        self.leave_node(kind);
        self.leave_ambient(decl.declare);
    }

    fn visit_ts_interface_declaration(&mut self, decl: &TSInterfaceDeclaration<'a>) {
        let decl = self.alloc(decl);
        let kind = AstKind::TSInterfaceDeclaration(decl);
        self.enter_node(kind);
        self.enter_ambient(decl.declare);
        let mut includes = SymbolFlags::Interface;
        if decl.declare {
            includes |= SymbolFlags::Ambient;
        }
        self.declare_symbol(decl.span, decl.id.name, includes, DeclSite::Other);
        self.enter_scope(ScopeFlags::empty(), &decl.scope_id);
        if let Some(type_parameters) = &decl.type_parameters {
            self.visit_ts_type_parameter_declaration(type_parameters);
        }
        self.visit_ts_interface_heritages(&decl.extends);
        self.visit_ts_interface_body(&decl.body);
        self.leave_scope();
        self.leave_node(kind);
        self.leave_ambient(decl.declare);
    }

    fn visit_ts_enum_declaration(&mut self, decl: &TSEnumDeclaration<'a>) {
        let decl = self.alloc(decl);
        let kind = AstKind::TSEnumDeclaration(decl);
        self.enter_node(kind);
        self.enter_ambient(decl.declare);
        let mut includes = if decl.r#const {
            SymbolFlags::ConstEnum
        } else {
            SymbolFlags::RegularEnum
        };
        if decl.declare {
            includes |= SymbolFlags::Ambient;
        }
        self.declare_symbol(decl.span, decl.id.name, includes, DeclSite::Enum);
        self.visit_ts_enum_body(&decl.body);
        self.leave_node(kind);
        self.leave_ambient(decl.declare);
    }

    fn visit_ts_enum_member(&mut self, member: &TSEnumMember<'a>) {
        let member = self.alloc(member);
        let kind = AstKind::TSEnumMember(member);
        self.enter_node(kind);
        let name =
            match &member.id {
                TSEnumMemberName::Identifier(ident) => ident.name,
                TSEnumMemberName::String(lit) | TSEnumMemberName::ComputedString(lit) => {
                    lit.value.as_str().map_or(Ident::empty(), Ident::from)
                }
                TSEnumMemberName::ComputedTemplateString(template) => template
                    .quasis
                    .first()
                    .map_or(Ident::empty(), |quasi| match quasi.value.cooked {
                        Some(cooked) => cooked.as_str().map_or(Ident::empty(), Ident::from),
                        None => Ident::from(quasi.value.raw),
                    }),
            };
        self.declare_symbol(member.span, name, SymbolFlags::EnumMember, DeclSite::Other);
        self.visit_ts_enum_member_name(&member.id);
        if let Some(initializer) = &member.initializer {
            self.visit_expression(initializer);
        }
        self.leave_node(kind);
    }

    fn visit_ts_type_parameter(&mut self, param: &TSTypeParameter<'a>) {
        let param = self.alloc(param);
        let kind = AstKind::TSTypeParameter(param);
        self.enter_node(kind);
        let mut scope = self.current_scope;
        if matches!(
            self.stack.get(self.stack.len().wrapping_sub(2)),
            Some(AstKind::TSInferType(_))
        ) {
            let mut candidate = Some(self.current_scope);
            while let Some(s) = candidate {
                if self.scope_flags(s).is_ts_conditional() {
                    scope = s;
                    break;
                }
                candidate = self.model.scopes[s as usize].parent;
            }
        }
        self.declare_symbol_on_scope(
            param.span,
            param.name.name,
            scope,
            SymbolFlags::TypeParameter,
            DeclSite::Other,
        );
        if let Some(constraint) = &param.constraint {
            self.visit_ts_type(constraint);
        }
        if let Some(default) = &param.default {
            self.visit_ts_type(default);
        }
        self.leave_node(kind);
    }

    fn visit_ts_mapped_type(&mut self, it: &TSMappedType<'a>) {
        let it = self.alloc(it);
        let kind = AstKind::TSMappedType(it);
        self.enter_node(kind);
        self.enter_scope(ScopeFlags::empty(), &it.scope_id);
        self.declare_symbol(
            it.span,
            it.key.name,
            SymbolFlags::TypeParameter,
            DeclSite::Other,
        );
        self.visit_ts_type(&it.constraint);
        if let Some(name_type) = &it.name_type {
            self.visit_ts_type(name_type);
        }
        if let Some(type_annotation) = &it.type_annotation {
            self.visit_ts_type(type_annotation);
        }
        self.leave_scope();
        self.leave_node(kind);
    }
}

fn var_head_declarator<'a>(left: &ForStatementLeft<'a>) -> Option<*const VariableDeclarator<'a>> {
    match left {
        ForStatementLeft::VariableDeclaration(decl)
            if decl.kind == VariableDeclarationKind::Var =>
        {
            decl.declarations.first().map(|d| d as *const _)
        }
        _ => None,
    }
}

/// Differential harness: every CompileState query answered by both backends
/// over the same program, compared by content (ids differ by construction).
#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    use super::*;
    use crate::imports::ImportTable;
    use crate::options::ResolvedOptions;
    use crate::state::CompileState;

    struct Collector<'a> {
        refs: Vec<&'a IdentifierReference<'a>>,
        sites: Vec<(&'a Cell<NodeId>, u32)>,
    }

    impl<'a> Visit<'a> for Collector<'a> {
        fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
            self.refs.push(self.alloc(it));
        }

        fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
            let it = self.alloc(it);
            self.sites.push((&it.node_id, it.span.start));
            walk::walk_call_expression(self, it);
        }

        fn visit_static_member_expression(&mut self, it: &StaticMemberExpression<'a>) {
            let it = self.alloc(it);
            self.sites.push((&it.node_id, it.span.start));
            walk::walk_static_member_expression(self, it);
        }

        fn visit_computed_member_expression(&mut self, it: &ComputedMemberExpression<'a>) {
            let it = self.alloc(it);
            self.sites.push((&it.node_id, it.span.start));
            walk::walk_computed_member_expression(self, it);
        }

        fn visit_jsx_opening_element(&mut self, it: &JSXOpeningElement<'a>) {
            let it = self.alloc(it);
            self.sites.push((&it.node_id, it.span.start));
            walk::walk_jsx_opening_element(self, it);
        }
    }

    const PROBES: [&str; 7] = [
        "stylex", "_stylex", "_temp", "_styles", "_inject", "React", "x",
    ];

    /// One query surface, rendered as sorted text lines so a diff names the
    /// exact query and construct.
    fn snapshot(
        state: &CompileState<'_>,
        collector: &Collector<'_>,
        names: &[String],
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let label = |decl: BindingDecl<'_>| match decl {
            BindingDecl::Declarator(_) => "VariableDeclarator",
            BindingDecl::NamedImport => "NamedImport",
            BindingDecl::DefaultImport => "DefaultImport",
            BindingDecl::NamespaceImport => "NamespaceImport",
            BindingDecl::Opaque(type_name) => type_name,
        };
        for id in &collector.refs {
            let binding = state.symbol_of(id).map(|symbol| {
                let info = state.binding_info(symbol);
                format!(
                    "{} {}..{} violated={} mutated={}",
                    label(info.decl),
                    info.span.start,
                    info.span.end,
                    state.is_non_constant(symbol),
                    state.is_mutated(symbol)
                )
            });
            lines.push(format!(
                "ref {}@{} -> {:?}",
                id.name, id.span.start, binding
            ));
        }
        for (name, kind, span) in state.debug_symbols() {
            lines.push(format!("symbol {name} {kind} {}..{}", span.start, span.end));
        }
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let parents = RefCell::new(Vec::new());
        let starts = state.reference_starts_where(&name_refs, |name, site| {
            parents
                .borrow_mut()
                .push(format!("parent {name} {:?}", site.parent()));
            true
        });
        lines.push(format!("starts {starts:?}"));
        lines.extend(parents.into_inner());
        for name in names {
            lines.push(format!(
                "uid {name} taken={} root={}",
                state.uid_name_taken(name),
                state.root_binding(name).is_some()
            ));
        }
        let sampled: Vec<&String> = names
            .iter()
            .step_by(names.len().div_ceil(40).max(1))
            .collect();
        for (cell, start) in &collector.sites {
            let node = cell.get();
            lines.push(format!(
                "site @{start} program_level={} stmt={}",
                state.is_program_level(node),
                state.program_statement_start(node)
            ));
            for name in &sampled {
                lines.push(format!(
                    "site @{start} {name} any={} root={}",
                    state.any_binding_at(node, name),
                    state.resolves_to_root_binding(node, name)
                ));
            }
        }
        lines.sort();
        lines
    }

    /// Lines the two backends disagree on for `source` (empty = parity).
    pub(crate) fn diff_backends(source: &str) -> Vec<String> {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, source, SourceType::tsx()).parse();
        if ret.fatal_error || ret.diagnostics.has_errors() {
            return Vec::new();
        }
        let program = &ret.program;
        let mut collector = Collector {
            refs: Vec::new(),
            sites: Vec::new(),
        };
        collector.visit_program(program);
        let options = ResolvedOptions::default();
        let build = |backend| {
            CompileState::build_with_imports_using(
                program,
                &options,
                None,
                String::new(),
                ImportTable::default(),
                backend,
            )
        };
        let oxc = build(ScopeBackend::Oxc);
        let mut names: BTreeSet<String> =
            collector.refs.iter().map(|r| r.name.to_string()).collect();
        names.extend(oxc.debug_symbols().into_iter().map(|(name, _, _)| name));
        names.extend(PROBES.iter().map(|p| p.to_string()));
        let names: Vec<String> = names.into_iter().collect();
        let expected = snapshot(&oxc, &collector, &names);
        drop(oxc);
        let native = build(ScopeBackend::Native);
        let actual = snapshot(&native, &collector, &names);
        let expected_set: BTreeSet<&String> = expected.iter().collect();
        let actual_set: BTreeSet<&String> = actual.iter().collect();
        let mut out: Vec<String> = expected_set
            .difference(&actual_set)
            .map(|line| format!("oxc only:    {line}"))
            .collect();
        out.extend(
            actual_set
                .difference(&expected_set)
                .map(|line| format!("native only: {line}")),
        );
        out
    }

    fn assert_parity(label: &str, source: &str) {
        let diff = diff_backends(source);
        assert!(diff.is_empty(), "{label}:\n{}", diff.join("\n"));
    }

    /// An empty diff must mean parity, not an empty snapshot.
    #[test]
    fn harness_records_every_query_kind() {
        let source = "import * as stylex from '@stylexjs/stylex'; const s = stylex.create({});\n\
function f() { return stylex.props(s.a); }";
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, source, SourceType::tsx()).parse();
        let mut collector = Collector {
            refs: Vec::new(),
            sites: Vec::new(),
        };
        collector.visit_program(&ret.program);
        let options = ResolvedOptions::default();
        let state = CompileState::build_with_imports_using(
            &ret.program,
            &options,
            None,
            String::new(),
            ImportTable::default(),
            ScopeBackend::Native,
        );
        let names = vec!["stylex".to_string(), "s".to_string()];
        let lines = snapshot(&state, &collector, &names);
        let has = |prefix: &str| lines.iter().any(|l| l.starts_with(prefix));
        assert!(
            has("ref stylex@54 -> Some(\"NamespaceImport 7..18"),
            "{lines:#?}"
        );
        assert!(has("symbol s VariableDeclarator 50..71"), "{lines:#?}");
        assert!(
            has("parent stylex Member { property: Some(\"create\") }"),
            "{lines:#?}"
        );
        assert!(has("uid stylex taken=true root=true"), "{lines:#?}");
        assert!(has("site @54 program_level=true stmt=44"), "{lines:#?}");
        assert!(
            lines.iter().any(|l| l.contains("program_level=false")),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.contains(" s any=true root=true")),
            "{lines:#?}"
        );
    }

    #[test]
    fn native_matches_oxc_on_binding_shapes() {
        let snippets: [(&str, &str); 26] = [
            (
                "var hoisting",
                "{ var a = 1; } { var a = 2; } function f() { { var b; } var b; b; } a; b;",
            ),
            (
                "catch clauses",
                "try {} catch (e) { var e; e; } try {} catch ({ m }) { m; } try {} catch { x; }",
            ),
            (
                "function expression name",
                "const f = function n(n) { return n; }; (function g(g) {}); f; n;",
            ),
            (
                "params before body",
                "function f(a = b, { c } = d) { var b, d; a; c; } const g = (p = q) => { let q; p; };",
            ),
            (
                "nested params resume outward",
                "const b = 0; function outer() { function f(a = b, x = y) { var b, y; } var b; } let y; class K { m(@dec(z) z) { var dec; } }",
            ),
            (
                "catch and type parameters",
                "try {} catch ({ m = n }) { var n; } function t<T extends U>(v: T): T { type U = 1; return v; } type U = 2; function a() { arguments; }",
            ),
            (
                "class shapes",
                "class A extends B { static x = A; [k]() {} } const C = class D extends A {}; D; new A();",
            ),
            (
                "ts declarations",
                "type T = number; interface I { x: T } enum E { A, B = A } namespace N { export const v = 1; } declare const d: E; N.v; E.A; d;",
            ),
            (
                "type-only references",
                "const v = 1; type Q = typeof v; type R = v; let w: Q; import type { TT } from './t'; TT; w;",
            ),
            (
                "exports",
                "const a = 1, b = 2; export { a, b as c }; export default a; export type { T }; type T = 1;",
            ),
            (
                "writes",
                "let a = 1, b = 2, c = 3, d = {}, e = []; a = 2; b += 1; c++; d.x = 1; e.push(1); [a, b] = [b, a]; ({ a } = d); for (a of e) {} for (b in d) {}",
            ),
            (
                "mutations through parens",
                "const u = {}; (u).x = 1; const v = {}; Object.assign((v), {}); const w = {}; Object.assign((w as any), {}); const q = []; (q)[\"x\"] = 1; delete (u).y; (v.z)++;",
            ),
            (
                "member keys",
                "const o = {}, k = 'a'; o[k] = 1; o[k]; o.k; this[k] = 2; (o ? k : o).x = 1;",
            ),
            (
                "jsx",
                "import * as React from 'react'; import { Foo } from './f'; const el = <Foo.Bar a={x} {...rest}><div>{y}</div></Foo.Bar>; <foo />; <this.Comp />;",
            ),
            (
                "nested statements",
                "if (a) f(); else { g(); } for (;;) h(); while (x) { i(); } label: j(); switch (k) { case 1: l(); } try { m(); } finally { n(); }",
            ),
            (
                "stylex shapes",
                "import * as stylex from '@stylexjs/stylex'; const s = stylex.create({ a: { color: 'red' } }); function C() { const t = stylex.create({}); return <div {...stylex.props(s.a, t)} />; }",
            ),
            (
                "shadowed stylex",
                "import * as stylex from '@stylexjs/stylex'; function f(stylex) { stylex.create({}); } { const stylex = 1; stylex.create({}); }",
            ),
            (
                "labels and breaks",
                "outer: for (const a of b) { inner: for (const c of d) { if (c) break outer; continue inner; } }",
            ),
            (
                "generators and async",
                "async function* g() { yield await p; } const h = async () => { for await (const x of y) {} };",
            ),
            (
                "destructuring params",
                "function f({ a, b: [c, ...d] }, ...rest) { a; c; d; rest; } const g = ([p, { q = r }]) => p + q;",
            ),
            (
                "type params",
                "function f<T extends U, V = T>(a: T): V { return a as any; } type M<K extends string> = { [P in K]: P }; type C<X> = X extends infer R ? R : never;",
            ),
            (
                "declare contexts",
                "declare class A extends B {} declare module 'm' { export const x: number; } declare global { interface Window { w: 1 } } abstract class Q { abstract [k](): void; declare [j]: 1; }",
            ),
            (
                "import equals",
                "import fs = require('fs'); import n = N.M; namespace N { export namespace M {} } fs; n;",
            ),
            (
                "template and tagged",
                "const t = `${a} ${b.c}`; tag`x${d}`; const { [e]: f } = g;",
            ),
            (
                "switch scope",
                "switch (x) { case y: { let z = 1; z; } default: let w; w; } let v; v;",
            ),
            (
                "enum member names",
                "enum E { 'a b' = 1, [`c`] = 2, D } E['a b'];",
            ),
        ];
        for (label, source) in snippets {
            assert_parity(label, source);
        }
    }

    #[test]
    fn native_matches_oxc_on_corpora() {
        let mut sources = crate::transform::visitor::tests::corpus_sources();
        if let Ok(paths) = std::env::var("OJ_STYLEX_SCOPES_CORPUS") {
            for path in paths.split(':').filter(|p| !p.is_empty()) {
                let text = std::fs::read_to_string(path).expect("corpus ndjson");
                for line in text.lines() {
                    let job: serde_json::Value = serde_json::from_str(line).unwrap();
                    let Some(source) = job.get("source").and_then(|s| s.as_str()) else {
                        continue;
                    };
                    let id = job["id"].as_str().unwrap_or("?");
                    sources.push((format!("{path}:{id}"), source.to_string()));
                }
            }
        }
        if sources.is_empty() {
            eprintln!("skipping native_matches_oxc_on_corpora: conformance corpus not vendored");
            return;
        }
        let mut failures = Vec::new();
        let mut checked = 0usize;
        for (filename, source) in &sources {
            let diff = diff_backends(source);
            checked += 1;
            if !diff.is_empty() {
                failures.push(format!("{filename}:\n  {}", diff.join("\n  ")));
            }
        }
        eprintln!(
            "scope parity: {checked} files, {} with differences",
            failures.len()
        );
        assert!(
            failures.is_empty(),
            "{} files differ:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
