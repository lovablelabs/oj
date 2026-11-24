//! Native binding analysis: CompileState's queries from one Visit walk that
//! records scopes, bindings and references, and looks a reference up only when
//! a query asks about it. The default; `OJ_STYLEX_NATIVE_SCOPE=0` keeps oxc_semantic.
// parity: oxc_semantic 0.158 SemanticBuilder + Binder (declaration, hoisting, flags, resolution).

use std::cell::Cell;

use oxc_allocator::{Address, GetAddress, UnstableAddress};
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

use crate::fxhash::FxHashMap;
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
    /// The native walk, unless `OJ_STYLEX_NATIVE_SCOPE=0` asks for oxc_semantic.
    pub fn from_env() -> Self {
        if oj_env::get().knobs.stylex_native_scope_off {
            ScopeBackend::Oxc
        } else {
            ScopeBackend::Native
        }
    }
}

type Idx = u32;

const ROOT: Idx = 0;
/// No parent scope / end of a binding list.
const NONE: Idx = u32::MAX;
/// [`Reference::symbol`] before its lookup ran.
const PENDING: Idx = u32::MAX;
/// [`Reference::symbol`] once a lookup found no binding.
const UNRESOLVED: Idx = u32::MAX - 1;

const CONSTNESS_KNOWN: u8 = 1;
const VIOLATED: u8 = 2;
const MUTATED: u8 = 4;

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
    /// [`CONSTNESS_KNOWN`] once computed, with [`VIOLATED`] and [`MUTATED`].
    constness: Cell<u8>,
}

struct Reference<'a> {
    name: Ident<'a>,
    start: u32,
    /// Where lookup starts: the scope holding the reference, or the parent of
    /// the function whose parameters it sits in and did not resolve against.
    scope: Idx,
    /// A symbol index, [`PENDING`] or [`UNRESOLVED`].
    symbol: Cell<Idx>,
    /// Rewritten by the lookup that resolves the reference.
    flags: Cell<ReferenceFlags>,
    mutation: bool,
    parent: RefParent<'a>,
}

struct Scope {
    parent: Idx,
    /// Head of this scope's list in [`ScopeModel::bindings`] (the root scope
    /// keeps a map instead).
    bindings: Idx,
    /// oxc `hoisting_variables`: `var` names that passed through this scope.
    hoisted: Idx,
    flags: ScopeFlags,
    /// Union of [`name_bit`] over `bindings`: a clear bit skips the scan.
    mask: u64,
}

struct Binding<'a> {
    name: Ident<'a>,
    symbol: Idx,
    next: Idx,
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

/// Where one upward lookup ended.
enum Lookup {
    Found(Idx, ReferenceFlags),
    Unresolved,
    /// Nothing through the last scope it was allowed to search.
    PastLast,
}

// parity: SemanticBuilder try_resolve_reference — whether a reference with
// `flags` may bind to a symbol with `symbol_flags`, and its flags once it has.
fn resolved_flags(flags: ReferenceFlags, symbol_flags: SymbolFlags) -> Option<ReferenceFlags> {
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
        return None;
    }
    if (symbol_flags.is_value() && flags.is_value()) || (flags.is_namespace() && flags.is_read()) {
        Some(flags - ReferenceFlags::Type)
    } else {
        Some(ReferenceFlags::Type)
    }
}

pub struct ScopeModel<'a> {
    scopes: Vec<Scope>,
    /// Every non-root scope's `bindings` and `hoisted` lists, linked by `next`.
    bindings: Vec<Binding<'a>>,
    root_bindings: IdentHashMap<'a, Idx>,
    symbols: Vec<Symbol<'a>>,
    refs: Vec<Reference<'a>>,
    /// References that write or mutate: all that constness has to look up.
    effects: Vec<Idx>,
    /// Indexed by the `NodeId` written into call/member/JSX-opening cells.
    sites: Vec<Site>,
}

impl<'a> ScopeModel<'a> {
    pub fn build(program: &'a Program<'a>) -> Self {
        let mut builder = Builder::new(program);
        builder.visit_program(program);
        builder.model
    }

    fn list_find(&self, mut entry: Idx, name: Ident<'_>) -> Option<Idx> {
        while entry != NONE {
            let binding = &self.bindings[entry as usize];
            if binding.name == name {
                return Some(binding.symbol);
            }
            entry = binding.next;
        }
        None
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
        self.list_find(s.bindings, name)
    }

    fn find_binding(&self, mut scope: Idx, name: Ident<'_>) -> Option<Idx> {
        let bit = name_bit(name);
        while scope != NONE {
            if let Some(symbol) = self.get_binding_masked(scope, name, bit) {
                return Some(symbol);
            }
            scope = self.scopes[scope as usize].parent;
        }
        None
    }

    /// Walks up from `scope` through `last` ([`NONE`] = the root) for a
    /// binding a reference to `name` with `flags` may resolve to.
    // parity: SemanticBuilder walk_up_resolve_reference + resolve_arguments_reference
    // — a value `arguments` stops at the nearest non-arrow function, whose
    // implicit arguments object has no symbol, and skips a function
    // expression's own name on the way.
    fn walk_up(&self, name: Ident<'_>, flags: ReferenceFlags, mut scope: Idx, last: Idx) -> Lookup {
        let bit = name_bit(name);
        let arguments = name.as_str() == "arguments"
            && flags.intersects(ReferenceFlags::Value | ReferenceFlags::ValueAsType);
        loop {
            if let Some(symbol) = self.get_binding_masked(scope, name, bit) {
                let symbol_flags = self.symbols[symbol as usize].flags;
                if !(arguments && symbol_flags.contains(SymbolFlags::FunctionExpression))
                    && let Some(resolved) = resolved_flags(flags, symbol_flags)
                {
                    return Lookup::Found(symbol, resolved);
                }
            }
            let s = &self.scopes[scope as usize];
            if arguments && s.flags.is_function() && !s.flags.is_arrow() {
                return Lookup::Unresolved;
            }
            if scope == last {
                return Lookup::PastLast;
            }
            if s.parent == NONE {
                return Lookup::Unresolved;
            }
            scope = s.parent;
        }
    }

    /// The reference's symbol or [`UNRESOLVED`], looked up on first use.
    fn resolve(&self, reference: &Reference<'a>) -> Idx {
        let known = reference.symbol.get();
        if known != PENDING {
            return known;
        }
        let symbol =
            match self.walk_up(reference.name, reference.flags.get(), reference.scope, NONE) {
                Lookup::Found(symbol, flags) => {
                    reference.flags.set(flags);
                    symbol
                }
                Lookup::Unresolved | Lookup::PastLast => UNRESOLVED,
            };
        reference.symbol.set(symbol);
        symbol
    }

    fn site(&self, node: NodeId) -> &Site {
        debug_assert!(node.index() != 0, "untagged node queried");
        &self.sites[node.index()]
    }

    pub fn symbol_of(&self, id: &IdentifierReference<'a>) -> Option<SymbolId> {
        let reference = self.refs.get(id.reference_id.get()?.index())?;
        let symbol = self.resolve(reference);
        (symbol != UNRESOLVED).then(|| SymbolId::from_usize(symbol as usize))
    }

    /// (constantViolations non-empty, isMutated) for one symbol.
    // parity: state.rs non_constant_flags.
    pub fn constness(&self, symbol: SymbolId) -> (bool, bool) {
        let s = &self.symbols[symbol.index()];
        let known = s.constness.get();
        if known != 0 {
            return (known & VIOLATED != 0, known & MUTATED != 0);
        }
        let target = symbol.index() as Idx;
        let mut violated = s.redeclared || s.var_for_head;
        let mut mutated = false;
        for &index in &self.effects {
            let reference = &self.refs[index as usize];
            if reference.name != s.name || self.resolve(reference) != target {
                continue;
            }
            if reference.flags.get().is_write() {
                violated = true;
            } else if reference.mutation {
                mutated = true;
            }
            if violated && mutated {
                break;
            }
        }
        s.constness.set(
            CONSTNESS_KNOWN
                | if violated { VIOLATED } else { 0 }
                | if mutated { MUTATED } else { 0 },
        );
        (violated, mutated)
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

    /// A name is taken by any symbol and by any unresolved reference; a
    /// resolved reference shares its symbol's name, so no lookup is needed.
    pub fn uid_name_taken(&self, name: &str) -> bool {
        let name = Ident::from(name);
        self.symbols.iter().any(|s| s.name == name) || self.refs.iter().any(|r| r.name == name)
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

struct Frame<'a> {
    kind: AstKind<'a>,
    /// Whether this node counts toward [`Builder::opaque_depth`].
    opaque: bool,
}

struct Builder<'a> {
    model: ScopeModel<'a>,
    current_scope: Idx,
    /// Ancestors, `Program` first; the top is the parent of the node being visited.
    stack: Vec<Frame<'a>>,
    /// Ancestors that make a node non-program-level (functions, nested statements).
    opaque_depth: u32,
    top_stmt_start: u32,
    reference_flags: ReferenceFlags,
    /// References recorded while a parameter region is open, for the lookup
    /// that closes it; every other reference waits for a query.
    pending: Vec<Idx>,
    open_parameter_regions: u32,
    ambient_depth: u32,
    typescript: bool,
    var_decl: Option<(VariableDeclarationKind, bool)>,
    var_head: Option<*const VariableDeclarator<'a>>,
    import_is_type: bool,
    catch_clause_span: Span,
    /// Whether a namespace body or statement instantiates, by node address.
    module_states: FxHashMap<Address, bool>,
}

impl<'a> Builder<'a> {
    fn new(program: &Program<'a>) -> Self {
        let source_len = program.source_text.len();
        let per_kb = |n: usize| ((source_len * n) / 1024).max(16);
        let mut root_flags = ScopeFlags::Top;
        if program.source_type.is_strict() || program.has_use_strict_directive() {
            root_flags |= ScopeFlags::StrictMode;
        }
        let mut scopes = Vec::with_capacity(per_kb(6));
        scopes.push(Scope {
            parent: NONE,
            bindings: NONE,
            hoisted: NONE,
            flags: root_flags,
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
                bindings: Vec::with_capacity(per_kb(4)),
                root_bindings: IdentHashMap::with_capacity_and_hasher(per_kb(3), IdentBuildHasher),
                symbols: Vec::with_capacity(per_kb(4)),
                refs: Vec::with_capacity(per_kb(20)),
                effects: Vec::new(),
                sites,
            },
            current_scope: ROOT,
            stack: Vec::with_capacity(64),
            opaque_depth: 0,
            top_stmt_start: 0,
            reference_flags: ReferenceFlags::empty(),
            pending: Vec::new(),
            open_parameter_regions: 0,
            ambient_depth: u32::from(program.source_type.is_typescript_definition()),
            typescript: program.source_type.is_typescript(),
            var_decl: None,
            var_head: None,
            import_is_type: false,
            catch_clause_span: Span::default(),
            module_states: FxHashMap::default(),
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
            return;
        }
        let bit = name_bit(name);
        let s = &self.model.scopes[scope as usize];
        if s.mask & bit != 0 {
            let mut entry = s.bindings;
            while entry != NONE {
                let binding = &mut self.model.bindings[entry as usize];
                if binding.name == name {
                    binding.symbol = symbol;
                    return;
                }
                entry = binding.next;
            }
        }
        let s = &mut self.model.scopes[scope as usize];
        let entry = self.model.bindings.len() as Idx;
        self.model.bindings.push(Binding {
            name,
            symbol,
            next: s.bindings,
        });
        s.bindings = entry;
        s.mask |= bit;
    }

    /// Unlinks `name` from `scope`, returning the symbol it was bound to.
    fn remove_binding(&mut self, scope: Idx, name: Ident<'a>) -> Option<Idx> {
        if scope == ROOT {
            return self.model.root_bindings.remove(&name);
        }
        let mut previous = NONE;
        let mut entry = self.model.scopes[scope as usize].bindings;
        while entry != NONE {
            let binding = &self.model.bindings[entry as usize];
            let (symbol, next) = (binding.symbol, binding.next);
            if binding.name == name {
                if previous == NONE {
                    self.model.scopes[scope as usize].bindings = next;
                } else {
                    self.model.bindings[previous as usize].next = next;
                }
                return Some(symbol);
            }
            previous = entry;
            entry = next;
        }
        None
    }

    fn hoisted(&self, scope: Idx, name: Ident<'a>) -> Option<Idx> {
        self.model
            .list_find(self.model.scopes[scope as usize].hoisted, name)
    }

    fn set_hoisted(&mut self, scope: Idx, name: Ident<'a>, symbol: Idx) {
        let mut entry = self.model.scopes[scope as usize].hoisted;
        while entry != NONE {
            let binding = &mut self.model.bindings[entry as usize];
            if binding.name == name {
                binding.symbol = symbol;
                return;
            }
            entry = binding.next;
        }
        let s = &mut self.model.scopes[scope as usize];
        let entry = self.model.bindings.len() as Idx;
        self.model.bindings.push(Binding {
            name,
            symbol,
            next: s.hoisted,
        });
        s.hoisted = entry;
    }

    // parity: SemanticBuilder check_redeclaration — a function expression's
    // own name never counts; otherwise the binding, else a `var` hoisted
    // through the scope.
    fn check_redeclaration(&self, scope: Idx, name: Ident<'a>) -> Option<Idx> {
        match self.model.get_binding(scope, name) {
            Some(symbol)
                if self.model.symbols[symbol as usize]
                    .flags
                    .contains(SymbolFlags::FunctionExpression) =>
            {
                None
            }
            Some(symbol) => Some(symbol),
            None => self.hoisted(scope, name),
        }
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
            constness: Cell::new(0),
        });
        self.add_binding(scope, name, symbol);
        symbol
    }

    // parity: SemanticBuilder declare_symbol_on_scope — a `var` hoisted
    // through the scope is bound further out, so only a value joins it; a
    // type declares its own symbol here.
    fn declare_symbol_on_scope(
        &mut self,
        span: Span,
        name: Ident<'a>,
        scope: Idx,
        includes: SymbolFlags,
        decl: DeclSite<'a>,
    ) -> Idx {
        if let Some(symbol) = self.check_redeclaration(scope, name)
            && (includes.intersects(SymbolFlags::Value) || self.scope_has_binding(scope, name))
        {
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
        // The scopes from here up to (excluding) the var scope are the ones
        // the name hoists through; the root is a var scope, so one is found.
        let innermost = self.current_scope;
        let mut target = innermost;
        while !self.scope_flags(target).is_var() {
            target = self.model.scopes[target as usize].parent;
        }
        bound_names(&decl.id, &mut |ident| {
            let name = ident.name;
            let mut declared = None;
            let mut scope = innermost;
            while scope != target {
                // A type-only binding lives in its own declaration space: a
                // hoisted `var` does not merge into it.
                if let Some(symbol) = self.check_redeclaration(scope, name)
                    && self.model.symbols[symbol as usize].flags.is_value()
                {
                    self.model.symbols[symbol as usize].redeclared = true;
                    declared = Some(symbol);
                    if !self.scope_has_binding(target, name) {
                        self.remove_binding(scope, name);
                        self.add_binding(target, name, symbol);
                    }
                    break;
                }
                scope = self.model.scopes[scope as usize].parent;
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
            let mut scope = innermost;
            while scope != target {
                self.set_hoisted(scope, name, symbol);
                scope = self.model.scopes[scope as usize].parent;
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
        let symbol = self.declare_symbol(
            func.span,
            ident.name,
            includes,
            DeclSite::Function {
                expression: func.is_expression(),
            },
        );
        // Annex B.3.2.1: a plain function declared in a sloppy-mode block is
        // also var-like in the enclosing function, unless the name is taken
        // there. JavaScript only.
        let block = self.current_scope;
        let block_flags = self.scope_flags(block);
        if self.typescript
            || !func.is_declaration()
            || func.r#async
            || func.generator
            || block_flags.is_var()
            || block_flags.is_strict_mode()
        {
            return;
        }
        let mut var_scope = self.model.scopes[block as usize].parent;
        while var_scope != NONE && !self.scope_flags(var_scope).is_var() {
            var_scope = self.model.scopes[var_scope as usize].parent;
        }
        if var_scope == NONE || self.scope_has_binding(var_scope, ident.name) {
            return;
        }
        if let Some(moved) = self.remove_binding(block, ident.name) {
            self.add_binding(var_scope, ident.name, moved);
        }
        self.set_hoisted(block, ident.name, symbol);
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
            match stack[i - 1].kind {
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
        let parent = stack[i - 1].kind;
        if let Some((object_span, property)) = member_object_span(parent) {
            let info = RefParent::Member { property };
            if object_span != child_span {
                return (info, false);
            }
            let mut j = i - 1;
            let mut member_span = parent.span();
            while j > 0 {
                match stack[j - 1].kind {
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
            let mutation = match stack[j - 1].kind {
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

    fn is_within(&self, mut scope: Idx, ancestor: Idx) -> bool {
        while scope != NONE {
            if scope == ancestor {
                return true;
            }
            scope = self.model.scopes[scope as usize].parent;
        }
        false
    }

    /// Opens a parameter region (function parameters and signature types, or
    /// a catch parameter) in the scope just entered. Its references are the
    /// only ones looked up during the walk: they must be matched against the
    /// scope before the body declares anything in it.
    fn open_parameter_region(&mut self) -> usize {
        self.open_parameter_regions += 1;
        self.pending.len()
    }

    /// Looks up the references recorded since `checkpoint` against the scope
    /// the region opened in. A miss resumes from the parent scope, at the
    /// close of an enclosing region or on first query.
    // parity: SemanticBuilder resolve_references_for_current_scope.
    fn close_parameter_region(&mut self, checkpoint: usize) {
        self.open_parameter_regions -= 1;
        let enclosed = self.open_parameter_regions > 0;
        let current = self.current_scope;
        let parent = self.model.scopes[current as usize].parent;
        let mut write = checkpoint;
        for read in checkpoint..self.pending.len() {
            let index = self.pending[read];
            let reference = &self.model.refs[index as usize];
            // Parameter decorators sit outside this scope chain; leave them.
            if self.is_within(reference.scope, current) {
                match self.model.walk_up(
                    reference.name,
                    reference.flags.get(),
                    reference.scope,
                    current,
                ) {
                    Lookup::Found(symbol, flags) => {
                        reference.symbol.set(symbol);
                        reference.flags.set(flags);
                        continue;
                    }
                    Lookup::Unresolved => {
                        reference.symbol.set(UNRESOLVED);
                        continue;
                    }
                    Lookup::PastLast => self.model.refs[index as usize].scope = parent,
                }
            }
            if enclosed {
                self.pending[write] = index;
                write += 1;
            }
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
        let class_scope = self.model.scopes[function_scope as usize].parent;
        debug_assert!(
            class_scope != NONE,
            "parameter decorators are visited from a function scope"
        );
        self.current_scope = class_scope;
        self.visit_decorators(decorators);
        self.current_scope = function_scope;
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

    // parity: Binder get_module_instance_state and its helpers — a namespace
    // holding only types (or aliases of such) is a NamespaceModule. `anchor`
    // is the stack index of the node the alias search climbs out from, and
    // `stmts` every module-block statement seen so far in this computation.
    // The per-address memo is part of the behaviour: the first computation of
    // a nested statement's state is the one every later binder reads.
    fn namespace_instantiates(
        &mut self,
        decl: &'a TSNamespaceDeclaration<'a>,
        anchor: usize,
        stmts: &mut Vec<&'a Statement<'a>>,
    ) -> bool {
        let address = decl.unstable_address();
        if let Some(&state) = self.module_states.get(&address) {
            return state;
        }
        let state = match &decl.body {
            TSNamespaceDeclarationBody::TSModuleBlock(block) => {
                self.module_block_instantiates(&block.body, anchor, stmts)
            }
            TSNamespaceDeclarationBody::TSNamespaceDeclaration(inner) => {
                self.namespace_instantiates(inner, anchor, &mut Vec::new())
            }
        };
        self.module_states.insert(address, state);
        state
    }

    fn external_module_instantiates(
        &mut self,
        decl: &'a TSExternalModuleDeclaration<'a>,
        anchor: usize,
        stmts: &mut Vec<&'a Statement<'a>>,
    ) -> bool {
        let address = decl.unstable_address();
        if let Some(&state) = self.module_states.get(&address) {
            return state;
        }
        let Some(block) = &decl.body else {
            return true;
        };
        let state = self.module_block_instantiates(&block.body, anchor, stmts);
        self.module_states.insert(address, state);
        state
    }

    fn module_block_instantiates(
        &mut self,
        body: &'a oxc_allocator::Vec<'a, Statement<'a>>,
        anchor: usize,
        stmts: &mut Vec<&'a Statement<'a>>,
    ) -> bool {
        stmts.extend(body.iter());
        body.iter()
            .any(|stmt| self.statement_instantiates(stmt, anchor, stmts))
    }

    fn statement_instantiates(
        &mut self,
        stmt: &'a Statement<'a>,
        anchor: usize,
        stmts: &mut Vec<&'a Statement<'a>>,
    ) -> bool {
        let address = stmt.address();
        if let Some(&state) = self.module_states.get(&address) {
            return state;
        }
        let state = match stmt {
            Statement::TSInterfaceDeclaration(_)
            | Statement::TSTypeAliasDeclaration(_)
            | Statement::TSImportEqualsDeclaration(_) => false,
            Statement::ExportDefaultDeclaration(export) => !matches!(
                export.declaration,
                ExportDefaultDeclarationKind::TSInterfaceDeclaration(_)
            ),
            Statement::ExportDeclaration(export) => match &export.declaration {
                Declaration::TSExternalModuleDeclaration(module) => {
                    self.external_module_instantiates(module, anchor, stmts)
                }
                Declaration::TSNamespaceDeclaration(inner) => {
                    self.namespace_instantiates(inner, anchor, stmts)
                }
                decl => !decl.is_type(),
            },
            Statement::ExportNamedDeclaration(export) => export
                .specifiers
                .iter()
                .any(|specifier| self.alias_target_instantiates(specifier, anchor, stmts)),
            Statement::TSNamespaceDeclaration(inner) => {
                self.namespace_instantiates(inner, anchor, stmts)
            }
            Statement::TSExternalModuleDeclaration(module) => {
                self.external_module_instantiates(module, anchor, stmts)
            }
            _ => true,
        };
        self.module_states.insert(address, state);
        state
    }

    /// `export { x }` inside a namespace instantiates it when what `x` names
    /// does; the declaration is searched for outward, block by block.
    fn alias_target_instantiates(
        &mut self,
        specifier: &ExportSpecifier<'a>,
        mut anchor: usize,
        stmts: &[&'a Statement<'a>],
    ) -> bool {
        let ModuleExportName::IdentifierReference(local) = &specifier.local else {
            return true;
        };
        let name = local.name;
        let mut block = stmts.to_vec();
        loop {
            if let Some(&stmt) = block.iter().find(|stmt| statement_declares(stmt, name)) {
                // A re-exported import alias is ambiguous: read as instantiating.
                return matches!(stmt, Statement::TSImportEqualsDeclaration(_))
                    || self.statement_instantiates(stmt, anchor, &mut Vec::new());
            }
            let enclosing = self.stack[..anchor].iter().rposition(|frame| {
                matches!(
                    frame.kind,
                    AstKind::Program(_) | AstKind::TSModuleBlock(_) | AstKind::BlockStatement(_)
                )
            });
            let Some(index) = enclosing else {
                return true;
            };
            anchor = index;
            block.clear();
            match self.stack[index].kind {
                AstKind::Program(program) => block.extend(program.body.iter()),
                AstKind::TSModuleBlock(module) => block.extend(module.body.iter()),
                AstKind::BlockStatement(statement) => block.extend(statement.body.iter()),
                _ => unreachable!("rposition matched a block-like kind"),
            }
        }
    }
}

fn declaration_declares(decl: &VariableDeclaration<'_>, name: Ident<'_>) -> bool {
    let mut found = false;
    for declarator in &decl.declarations {
        bound_names(&declarator.id, &mut |ident| found |= ident.name == name);
    }
    found
}

fn statement_declares(stmt: &Statement<'_>, name: Ident<'_>) -> bool {
    let names = |id: Option<&BindingIdentifier<'_>>| id.is_some_and(|id| id.name == name);
    match stmt {
        Statement::VariableDeclaration(decl) => declaration_declares(decl, name),
        Statement::ExportDeclaration(export) => match &export.declaration {
            Declaration::VariableDeclaration(decl) => declaration_declares(decl, name),
            decl => names(decl.id()),
        },
        Statement::ExportDefaultDeclaration(export) => match &export.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(decl) => names(decl.id.as_ref()),
            ExportDefaultDeclarationKind::ClassDeclaration(decl) => names(decl.id.as_ref()),
            ExportDefaultDeclarationKind::TSInterfaceDeclaration(decl) => names(Some(&decl.id)),
            _ => false,
        },
        _ => stmt.as_declaration().is_some_and(|decl| names(decl.id())),
    }
}

// parity: SemanticBuilder is_delete_member_target — `delete` writes a property
// only when its argument, parens and assertions aside, is a member (an
// optional chain ending in one included).
fn is_delete_member_target(expr: &Expression<'_>) -> bool {
    match expr.get_inner_expression() {
        Expression::ChainExpression(chain) => matches!(
            chain.expression,
            ChainElement::StaticMemberExpression(_)
                | ChainElement::ComputedMemberExpression(_)
                | ChainElement::PrivateFieldExpression(_)
        ),
        inner => inner.is_member_expression(),
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
        let opaque = opaque_step(kind, self.stack.last().map(|frame| frame.kind));
        self.opaque_depth += u32::from(opaque);
        self.stack.push(Frame { kind, opaque });
    }

    fn leave_node(&mut self, _kind: AstKind<'a>) {
        if let Some(frame) = self.stack.pop() {
            self.opaque_depth -= u32::from(frame.opaque);
        }
    }

    // parity: Scoping get_new_scope_flags — strict mode is inherited.
    fn enter_scope(&mut self, flags: ScopeFlags, _scope_id: &Cell<Option<ScopeId>>) {
        let parent = self.current_scope;
        let inherited = self.model.scopes[parent as usize].flags & ScopeFlags::StrictMode;
        let index = self.model.scopes.len() as Idx;
        self.model.scopes.push(Scope {
            parent,
            bindings: NONE,
            hoisted: NONE,
            flags: flags | inherited,
            mask: 0,
        });
        self.current_scope = index;
    }

    fn leave_scope(&mut self) {
        let parent = self.model.scopes[self.current_scope as usize].parent;
        debug_assert!(parent != NONE, "leave_scope on the root scope");
        self.current_scope = parent;
    }

    fn visit_program(&mut self, program: &Program<'a>) {
        self.stack.push(Frame {
            kind: AstKind::Program(self.alloc(program)),
            opaque: false,
        });
        self.visit_statements(&program.body);
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
        let flags = self.take_reference_flags();
        let (parent, mutation) = self.reference_parent(ident.span);
        let index = self.model.refs.len() as Idx;
        self.model.refs.push(Reference {
            name: ident.name,
            start: ident.span.start,
            scope: self.current_scope,
            symbol: Cell::new(PENDING),
            flags: Cell::new(flags),
            mutation,
            parent,
        });
        ident
            .reference_id
            .set(Some(ReferenceId::from_usize(index as usize)));
        if flags.is_write() || mutation {
            self.model.effects.push(index);
        }
        if self.open_parameter_regions > 0 {
            self.pending.push(index);
        }
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
        if it.operator == UnaryOperator::Delete && is_delete_member_target(&it.argument) {
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
        if sig.computed {
            self.reference_flags = ReferenceFlags::ValueAsType;
        }
        self.visit_property_key(&sig.key);
        self.reference_flags = ReferenceFlags::empty();
        self.enter_scope(ScopeFlags::empty(), &sig.scope_id);
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

    fn visit_ts_type_assertion(&mut self, it: &TSTypeAssertion<'a>) {
        let kind = AstKind::TSTypeAssertion(self.alloc(it));
        self.enter_node(kind);
        // The type must not consume the asserted expression's reference flags.
        let flags = std::mem::take(&mut self.reference_flags);
        self.visit_ts_type(&it.type_annotation);
        self.reference_flags = flags;
        self.visit_expression(&it.expression);
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
        let strict = if func.has_use_strict_directive() {
            ScopeFlags::StrictMode
        } else {
            ScopeFlags::empty()
        };
        self.enter_scope(flags | strict, &func.scope_id);
        if func.is_expression() {
            self.bind_function(func);
        }
        let checkpoint = self.open_parameter_region();
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
        self.close_parameter_region(checkpoint);
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
        let mut flags = ScopeFlags::Function | ScopeFlags::Arrow;
        if expr.has_use_strict_directive() {
            flags |= ScopeFlags::StrictMode;
        }
        self.enter_scope(flags, &expr.scope_id);
        let checkpoint = self.open_parameter_region();
        if let Some(parameters) = &expr.type_parameters {
            self.visit_ts_type_parameter_declaration(parameters);
        }
        self.visit_formal_parameters(&expr.params);
        if let Some(return_type) = &expr.return_type {
            self.visit_ts_type_annotation(return_type);
        }
        self.close_parameter_region(checkpoint);
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
        let checkpoint = self.open_parameter_region();
        self.visit_binding_pattern(&param.pattern);
        if let Some(type_annotation) = &param.type_annotation {
            self.visit_ts_type_annotation(type_annotation);
        }
        self.close_parameter_region(checkpoint);
        self.leave_node(kind);
    }

    fn visit_block_statement(&mut self, it: &BlockStatement<'a>) {
        let kind = AstKind::BlockStatement(self.alloc(it));
        self.enter_node(kind);
        let parent_scope = self.current_scope;
        self.enter_scope(ScopeFlags::empty(), &it.scope_id);
        // A catch clause's parameter bindings move into its body's scope.
        if self.scope_flags(parent_scope).is_catch_clause() {
            let parent = &mut self.model.scopes[parent_scope as usize];
            let moved = std::mem::replace(&mut parent.bindings, NONE);
            let mask = std::mem::take(&mut parent.mask);
            let block = &mut self.model.scopes[self.current_scope as usize];
            block.bindings = moved;
            block.mask = mask;
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
        let anchor = self.stack.len() - 1;
        let mut includes = if self.namespace_instantiates(decl, anchor, &mut Vec::new()) {
            SymbolFlags::ValueModule
        } else {
            SymbolFlags::NamespaceModule
        };
        if decl.declare {
            includes |= SymbolFlags::Ambient;
        }
        self.declare_symbol(decl.span, decl.id.name, includes, DeclSite::Other);
        let mut flags = ScopeFlags::TsModuleBlock;
        if decl.has_use_strict_directive() {
            flags |= ScopeFlags::StrictMode;
        }
        self.enter_scope(flags, &decl.scope_id);
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
        // A name that is not UTF-8 (a lone surrogate) declares nothing.
        let name = match &member.id {
            TSEnumMemberName::Identifier(ident) => Some(ident.name),
            TSEnumMemberName::String(lit) | TSEnumMemberName::ComputedString(lit) => {
                lit.value.as_str().map(Ident::from)
            }
            TSEnumMemberName::ComputedTemplateString(template) => template
                .single_quasi()
                .and_then(|quasi| quasi.as_str())
                .map(Ident::from),
        };
        if let Some(name) = name {
            self.declare_symbol(member.span, name, SymbolFlags::EnumMember, DeclSite::Other);
        }
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
            Some(Frame {
                kind: AstKind::TSInferType(_),
                ..
            })
        ) {
            let mut candidate = self.current_scope;
            while candidate != NONE {
                if self.scope_flags(candidate).is_ts_conditional() {
                    scope = candidate;
                    break;
                }
                candidate = self.model.scopes[candidate as usize].parent;
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

    /// Source types whose binding rules differ: TS modules (fru parses
    /// everything as tsx; plain ts admits `<T>x` assertions), JS modules,
    /// sloppy scripts and CommonJS (Annex B function hoisting) and
    /// declaration files (ambient everywhere).
    fn source_types() -> [(&'static str, SourceType); 6] {
        [
            ("tsx", SourceType::tsx()),
            ("ts", SourceType::ts()),
            ("jsx module", SourceType::jsx()),
            ("jsx script", SourceType::jsx().with_script(true)),
            ("cjs", SourceType::cjs()),
            ("d.ts", SourceType::d_ts()),
        ]
    }

    /// Lines the two backends disagree on for `source` parsed as
    /// `source_type` (empty = parity); `None` when it does not parse.
    fn diff_backends(source: &str, source_type: SourceType) -> Option<Vec<String>> {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, source, source_type).parse();
        if ret.fatal_error || ret.diagnostics.has_errors() {
            return None;
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
        Some(out)
    }

    /// Parity under every source type `source` parses as (at least one).
    fn assert_parity(label: &str, source: &str) {
        let mut parsed = 0;
        for (type_label, source_type) in source_types() {
            let Some(diff) = diff_backends(source, source_type) else {
                continue;
            };
            parsed += 1;
            assert!(
                diff.is_empty(),
                "{label} ({type_label}):\n{}",
                diff.join("\n")
            );
        }
        assert!(parsed > 0, "{label}: parses under no source type");
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
        let snippets: &[(&str, &str)] = &[
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
            (
                "enum member lone surrogate",
                "enum E { '\\uD800' = 1, ['\\uDC00'] = 2, A = 3 } E.A;",
            ),
            (
                "arguments object",
                "function f() { arguments; const g = () => arguments; { arguments; } }\n\
                 function h(arguments) { arguments; }\n\
                 function k() { var arguments; arguments; (() => arguments)(); }\n\
                 function p(a = arguments, b = () => arguments) { let arguments; }\n\
                 const q = (a = arguments) => { var arguments; arguments; };\n\
                 var arguments; arguments; (() => arguments)();",
            ),
            (
                "arguments and function expression names",
                "(function arguments() { return arguments; });\n\
                 (function arguments() { return () => arguments; });\n\
                 (function f(arguments) { return arguments; });\n\
                 const o = { arguments() { return arguments; } };\n\
                 class K { static x = arguments; m() { return arguments; } }",
            ),
            (
                "arguments as a type",
                "type arguments = 1; function f() { let a: arguments; let b: typeof arguments; }\n\
                 function g<arguments>(x: arguments) { return arguments; }",
            ),
            (
                "method signature keys",
                "const k = 'a'; interface I { [k](k: number): void; [k]: string; [k]<k>(): k }\n\
                 type T = { [k](k: string): typeof k };",
            ),
            (
                "ambient class members",
                "import type { B } from 'b'; import type { k } from 'k';\n\
                 export class A extends B { [k]: string; [k](): void; }\n\
                 export declare class C extends B { [k]: string; }",
            ),
            (
                "var hoisted past types",
                "{ var x; interface x {} } type y = 0; { var y; } { type z = 1; var z; }\n\
                 { var w; type w = 2; let v; } x; y; z; w; let a: x, b: y, c: z, d: w;",
            ),
            (
                "var hoisted into values",
                "{ var q; function q() {} } { var c; class c {} } { var e; enum e {} }\n\
                 { { var n; } namespace n { export const v = 1; } } q; c; e; n;",
            ),
            (
                "catch parameter hoisting",
                "try {} catch (e) { var e = 1; { var e; } } e;\n\
                 function f() { try {} catch (e) { { var e; } } return e; }\n\
                 try {} catch ({ a = b }) { var b; a; } try {} catch (g) { let h = g; { g; } }",
            ),
            (
                "type assertion targets",
                "let a: any, b: any; type T = any; (<T>a) = 1; (<T>b).x = 2; (<T>a)++;\n\
                 [<T>a, <T>b] = [b, a]; for (<T>a of []) {}",
            ),
            (
                "delete targets",
                "const o: any = {}, p: any = {}, r: any = {}, s: any = {}, t: any = {}, u: any = {};\n\
                 delete o?.a; delete (p.b); delete (r as any).c; delete s?.[p]; delete t!.x;\n\
                 delete u?.f(); delete (u)?.g.h; delete u;",
            ),
            (
                "writes to type-only bindings",
                "interface X {} X = 1; type Y = 1; Y++; import type { Z } from 'z'; Z = 2;\n\
                 enum E {} E = 3; namespace N {} N = 4; namespace M { export const a = 1; } M = 5;\n\
                 import { V } from 'v'; V = 6; function outer(X, Y) { X = 1; Y += 1; }",
            ),
            (
                "namespace instantiation",
                "namespace A { export type T = 1; } namespace B { const enum E { X } }\n\
                 namespace C { interface I {} export { I }; }\n\
                 namespace D { import x = A; export { x }; }\n\
                 namespace F { export namespace G { export const v = 1; } }\n\
                 namespace F2 { export namespace G { export type V = 1; } }\n\
                 namespace P.Q.R { export type S = 1; } namespace P2.Q.R { export const s = 1; }\n\
                 A; B; C; D; F; F2; P; P2; let t: A.T, u: F2.G.V;",
            ),
            (
                "namespace export aliases",
                "const Outer = 1; type OuterT = 1; interface OuterI {}\n\
                 namespace H { export { Outer }; } namespace J { export { OuterT }; }\n\
                 namespace K { export { Missing }; } namespace L { export { OuterI as default }; }\n\
                 namespace M { namespace Inner { export { OuterT }; } export { Inner }; }\n\
                 namespace O { type Local = 1; namespace Deep { export { Local }; } }\n\
                 function scope() { type Local = 1; { namespace Q { export { Local }; } Q; } }\n\
                 H; J; K; L; M; O;",
            ),
            (
                "annex b function hoisting",
                "{ function f() {} } f; if (x) { function g() {} } g;\n\
                 function outer() { { function h() {} } h; let i; { function i() {} } i; }\n\
                 { async function a() {} } a; { function* b() {} } b;\n\
                 { function k() {} var k; } k; { { function deep() {} } } deep;\n\
                 switch (x) { case 1: function s() {} } s;\n\
                 try { function t() {} } catch (e) { function u() {} } t; u;\n\
                 label: { function v() {} } v;\n\
                 function strict() { 'use strict'; { function w() {} } w; }\n\
                 class C { m() { { function z() {} } z; } }\n\
                 const arrow = () => { 'use strict'; { function y() {} } y; };\n\
                 { function dup() {} } { function dup() {} } dup;",
            ),
            (
                "nested parameter regions",
                "function f(a = () => b, c = function () { var d; return d + b; }) { var b; }\n\
                 const g = (h = (i = j) => { var j; }) => { let j; }; var j;\n\
                 function o(p = function q(r = s) { var s; }) { var s; }\n\
                 function t<U>(v: U = (<W>(x: W, y = v) => x) as any): U { type W = 1; let y; return v; }\n\
                 try {} catch ({ k = (l = m) => l }) { var m; }",
            ),
            (
                "shadowing chains",
                "const a = 1; function f(a) { { let a; a = 2; } a++; return () => { var a; a.b = 1; }; }\n\
                 a; class a2 { a = a; static { var a; a = 3; } m(a = a) {} }\n\
                 for (let a = 0; a < 1; a++) { const b = a; } for (const a in {}) a;\n\
                 label: for (var a3 of []) { ((a3) => a3)(a3); }",
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
            for (type_label, source_type) in source_types() {
                let Some(diff) = diff_backends(source, source_type) else {
                    continue;
                };
                checked += 1;
                if !diff.is_empty() {
                    failures.push(format!(
                        "{filename} ({type_label}):\n  {}",
                        diff.join("\n  ")
                    ));
                }
            }
        }
        eprintln!(
            "scope parity: {} files, {checked} parses, {} with differences",
            sources.len(),
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
