// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! TanStack's dead-code pass, `deadCodeElimination(ast, refIdents)` from
//! babel-dead-code-elimination, which Start's compiler and the router's code
//! splitter run after their edits with `findReferencedIdentifiers` of the
//! module before them as candidates. Ported rule for rule:
//!
//! - candidates: the declared names (import specifiers, declarators,
//!   destructured names, function declarations, at every scope) that were
//!   read before the edit, minus the module bindings already removable then;
//! - `findRemovableBindings`, over the module's bindings only: a reference
//!   inside another binding's node is an edge from it, one anywhere else (an
//!   export included) keeps it, as do a reassignment or a loop iterator; what
//!   nothing kept reaches is removable (a self-recursive helper, helpers that
//!   only call each other);
//! - `shouldBeRemoved`, for a candidate: removable, or not read, never
//!   reassigned (redeclared counts), not a `for…of`/`for…in` iterator, and
//!   Babel's rest rule in an object pattern; a function declaration's name is
//!   looked up from inside the function, as Babel's scope does;
//! - removal: import specifiers (an import left with none goes), declarators
//!   (a `for` head's included), dead names in patterns, function
//!   declarations; any pattern left empty in a declarator goes, as Babel's
//!   pattern visitors remove it; classes stay; repeated until nothing goes.
//!
//! Edits only delete (a removed statement that is not in a statement list
//! becomes `;`) and keep the module's line count. A nested binding is known
//! by its offset, carried through each pass's edits; a module one by name,
//! as Babel's program scope holds one binding per name.

use std::collections::{HashMap, HashSet};

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    BindingIdentifier, BindingPattern, ImportDeclarationSpecifier, Program, Statement,
    VariableDeclaration,
};
use oxc_ast_visit::{walk, Visit};
use oxc_parser::Parser;
use oxc_semantic::{Scoping, Semantic, SemanticBuilder};
use oxc_span::{GetSpan, SourceType, Span};
use oxc_syntax::symbol::SymbolId;

/// `after` with what only the edit's removed code used gone, the candidates
/// being the bindings `before` read.
pub(crate) fn drop_dead_bindings(before: &str, mut after: String, path: &str) -> String {
    let Some(start) = Analysis::of(before, path, |a| a.start_candidates()) else {
        return after;
    };
    // No early exit without candidates: Babel's empty-pattern visitors run
    // regardless.
    let (module, nested_keys) = start;
    // The edit kept every line: a nested candidate is found again through
    // the part of its line the edit left as it was. One inside an edited
    // stretch stays unmatched, so it is kept (never another one dropped).
    let mapped: Vec<(String, u32)> = nested_keys
        .into_iter()
        .filter_map(|(name, at)| map_offset(before, &after, at).map(|to| (name, to)))
        .collect();
    let Some(mut nested) = Analysis::of(&after, path, |a| a.nested_at(&mapped)) else {
        return after;
    };
    while let Some((out, edits)) = drop_pass(&after, path, &module, &nested) {
        nested = remap(&nested, &edits);
        after = out;
    }
    after
}

fn source_type(path: &str) -> SourceType {
    SourceType::from_path(path.split('?').next().unwrap_or(path))
        .unwrap_or_else(|_| SourceType::tsx())
}

/// A declared name.
struct Decl {
    name: String,
    /// The binding identifier's start: a nested declaration's key.
    at: u32,
    symbol: SymbolId,
    /// Babel's `binding.path.node`: the import specifier, the declarator,
    /// the function or the class.
    container: Span,
    /// Declared in the module scope.
    module: bool,
    /// Inside an export declaration (at any depth: a namespace's included).
    exported: bool,
    /// A `for…of` / `for…in` head's (`isLoopIteratorBinding`).
    loop_iter: bool,
    is_class: bool,
}

/// One parse of a module: its declarations, references and the module
/// bindings `findRemovableBindings` finds removable.
struct Analysis<'s, 'a> {
    code: &'s str,
    scoping: &'s Scoping,
    decls: Vec<Decl>,
    reads: HashMap<SymbolId, Vec<Span>>,
    written: HashSet<SymbolId>,
    removable: HashSet<SymbolId>,
    by_at: HashMap<u32, usize>,
    program: &'s Program<'a>,
}

impl<'s, 'a> Analysis<'s, 'a> {
    /// `f` over the analysis of `code`, or None when it does not parse.
    fn of<T>(code: &str, path: &str, f: impl FnOnce(&Analysis) -> T) -> Option<T> {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, code, source_type(path)).parse();
        if parsed.fatal_error {
            return None;
        }
        let semantic = SemanticBuilder::new().build(&parsed.program).semantic;
        let analysis = Analysis::new(code, &parsed.program, &semantic);
        Some(f(&analysis))
    }

    fn new(code: &'s str, program: &'s Program<'a>, semantic: &'s Semantic<'a>) -> Self {
        let scoping = semantic.scoping();
        let root = scoping.root_scope_id();
        let spans = reference_spans(program);
        let mut collect = Collect {
            scoping,
            root,
            exported: 0,
            heads: HashSet::new(),
            out: Vec::new(),
        };
        collect.visit_program(program);
        let decls = collect.out;

        let mut reads: HashMap<SymbolId, Vec<Span>> = HashMap::new();
        let mut written = HashSet::new();
        let mut seen = HashSet::new();
        for d in &decls {
            if !seen.insert(d.symbol) {
                continue;
            }
            // A redeclaration is a constant violation in Babel's scope.
            if !scoping.symbol_redeclarations(d.symbol).is_empty() {
                written.insert(d.symbol);
            }
            for &rid in scoping.get_resolved_reference_ids(d.symbol) {
                let reference = scoping.get_reference(rid);
                if reference.is_write() {
                    written.insert(d.symbol);
                }
                if reference.is_read() || !reference.is_write() {
                    // An export specifier's local has no identifier node: it
                    // sits outside every binding and keeps it.
                    let span = spans
                        .get(&rid)
                        .copied()
                        .unwrap_or(Span::new(u32::MAX, u32::MAX));
                    reads.entry(d.symbol).or_default().push(span);
                }
            }
        }
        let by_at = decls.iter().enumerate().map(|(i, d)| (d.at, i)).collect();
        let mut analysis = Analysis {
            code,
            scoping,
            by_at,
            decls,
            reads,
            written,
            removable: HashSet::new(),
            program,
        };
        analysis.removable = analysis.find_removable();
        analysis
    }

    /// Babel's `binding.referenced`, for any binding (a parameter too).
    fn read(&self, symbol: SymbolId) -> bool {
        match self.reads.get(&symbol) {
            Some(r) => !r.is_empty(),
            None => self
                .scoping
                .get_resolved_reference_ids(symbol)
                .iter()
                .any(|&rid| {
                    let r = self.scoping.get_reference(rid);
                    r.is_read() || !r.is_write()
                }),
        }
    }

    /// Babel's `constantViolations` are not empty.
    fn reassigned(&self, symbol: SymbolId) -> bool {
        self.written.contains(&symbol)
            || (!self.reads.contains_key(&symbol)
                && (!self.scoping.symbol_redeclarations(symbol).is_empty()
                    || self
                        .scoping
                        .get_resolved_reference_ids(symbol)
                        .iter()
                        .any(|&rid| self.scoping.get_reference(rid).is_write())))
    }

    /// `findRemovableBindings` over the module's bindings.
    fn find_removable(&self) -> HashSet<SymbolId> {
        let module: Vec<&Decl> = self.decls.iter().filter(|d| d.module).collect();
        let mut alive: HashSet<SymbolId> = HashSet::new();
        let mut users: HashMap<SymbolId, Vec<SymbolId>> = HashMap::new();
        let mut queue = Vec::new();
        let mut symbols: Vec<SymbolId> = Vec::new();
        let mut seen = HashSet::new();
        for d in &module {
            if !seen.insert(d.symbol) {
                continue;
            }
            symbols.push(d.symbol);
            let keep = d.exported || d.loop_iter || self.written.contains(&d.symbol);
            if keep && alive.insert(d.symbol) {
                queue.push(d.symbol);
            }
        }
        // Module bindings' nodes never nest (an import specifier, a
        // module-scope declarator, a top-level function or class), so the one
        // holding a reference is found by its start.
        let mut containers: Vec<(Span, Vec<SymbolId>)> = Vec::new();
        let mut sorted: Vec<&Decl> = module.clone();
        sorted.sort_by_key(|d| (d.container.start, d.container.end));
        for d in sorted {
            match containers.last_mut() {
                Some((span, symbols)) if *span == d.container => symbols.push(d.symbol),
                _ => containers.push((d.container, vec![d.symbol])),
            }
        }
        let holder = |span: Span| -> Option<&Vec<SymbolId>> {
            let i = containers.partition_point(|(c, _)| c.start <= span.start);
            let (c, symbols) = containers.get(i.checked_sub(1)?)?;
            (span.end <= c.end).then_some(symbols)
        };
        for &symbol in &symbols {
            for &span in self
                .reads
                .get(&symbol)
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                match holder(span) {
                    None => {
                        if alive.insert(symbol) {
                            queue.push(symbol);
                        }
                    }
                    Some(holders) => {
                        for &c in holders {
                            users.entry(c).or_default().push(symbol);
                        }
                    }
                }
            }
        }
        while let Some(c) = queue.pop() {
            for &target in users.get(&c).map(Vec::as_slice).unwrap_or_default() {
                if alive.insert(target) {
                    queue.push(target);
                }
            }
        }
        // A binding nothing reads is not "removable" in Babel's sense (no
        // self-loop): it goes through the plain unreferenced rule instead.
        symbols
            .into_iter()
            .filter(|s| !alive.contains(s) && self.read(*s))
            .collect()
    }

    /// The candidates: module names, and nested `(name, offset)`.
    fn start_candidates(&self) -> (HashSet<String>, Vec<(String, u32)>) {
        let mut module = HashSet::new();
        let mut nested = Vec::new();
        for d in &self.decls {
            if !self.read(d.symbol) || self.removable.contains(&d.symbol) {
                continue;
            }
            if d.module {
                module.insert(d.name.clone());
            } else {
                nested.push((d.name.clone(), d.at));
            }
        }
        (module, nested)
    }

    /// The nested declarations at `offsets` that bind `name`.
    fn nested_at(&self, mapped: &[(String, u32)]) -> HashSet<u32> {
        mapped
            .iter()
            .filter(|(name, at)| {
                self.decl_at(*at)
                    .is_some_and(|d| !d.module && d.name == *name)
            })
            .map(|(_, at)| *at)
            .collect()
    }

    fn decl_at(&self, at: u32) -> Option<&Decl> {
        self.by_at.get(&at).map(|&i| &self.decls[i])
    }
}

fn line_of(code: &str, at: u32) -> usize {
    code[..at as usize].matches('\n').count()
}

/// `at` in `before` as an offset in `after`, which has the same lines: exact
/// in the part of its line before the first change or after the last one,
/// None inside the changed stretch.
fn map_offset(before: &str, after: &str, at: u32) -> Option<u32> {
    let at = at as usize;
    let line = line_of(before, at as u32);
    let bounds = |code: &str| -> Option<(usize, usize)> {
        let start = if line == 0 {
            0
        } else {
            code.match_indices('\n').nth(line - 1)?.0 + 1
        };
        let end = code[start..].find('\n').map_or(code.len(), |e| start + e);
        Some((start, end))
    };
    let (bs, be) = bounds(before)?;
    let (as_, ae) = bounds(after)?;
    let (b, a) = (&before.as_bytes()[bs..be], &after.as_bytes()[as_..ae]);
    let prefix = b.iter().zip(a).take_while(|(x, y)| x == y).count();
    let room = b.len().min(a.len()) - prefix;
    let suffix = b
        .iter()
        .rev()
        .zip(a.iter().rev())
        .take(room)
        .take_while(|(x, y)| x == y)
        .count();
    let col = at - bs;
    if col < prefix {
        Some((as_ + col) as u32)
    } else if col >= b.len() - suffix {
        Some((as_ + a.len() - (b.len() - col)) as u32)
    } else {
        None
    }
}

/// Every declared name, at every depth.
struct Collect<'s> {
    scoping: &'s Scoping,
    root: oxc_syntax::scope::ScopeId,
    /// Depth inside export declarations.
    exported: usize,
    /// `for…of` / `for…in` heads, by start.
    heads: HashSet<u32>,
    out: Vec<Decl>,
}

impl Collect<'_> {
    fn push(&mut self, id: &BindingIdentifier, container: Span, loop_iter: bool, is_class: bool) {
        let Some(symbol) = id.symbol_id.get() else {
            return;
        };
        self.out.push(Decl {
            name: id.name.to_string(),
            at: id.span.start,
            symbol,
            container,
            module: self.scoping.symbol_scope_id(symbol) == self.root,
            exported: self.exported > 0,
            loop_iter,
            is_class,
        });
    }
}

impl<'a> Visit<'a> for Collect<'_> {
    fn visit_import_declaration(&mut self, it: &oxc_ast::ast::ImportDeclaration<'a>) {
        if !it.import_kind.is_type() {
            for s in it.specifiers.iter().flatten() {
                self.push(s.local(), s.span(), false, false);
            }
        }
    }
    fn visit_export_declaration(&mut self, it: &oxc_ast::ast::ExportDeclaration<'a>) {
        self.exported += 1;
        walk::walk_export_declaration(self, it);
        self.exported -= 1;
    }
    fn visit_export_default_declaration(
        &mut self,
        it: &oxc_ast::ast::ExportDefaultDeclaration<'a>,
    ) {
        self.exported += 1;
        walk::walk_export_default_declaration(self, it);
        self.exported -= 1;
    }
    fn visit_variable_declaration(&mut self, it: &VariableDeclaration<'a>) {
        let loop_iter = self.heads.contains(&it.span.start);
        for d in &it.declarations {
            for id in pattern_names(&d.id) {
                self.push(id, d.span, loop_iter, false);
            }
        }
        // Inside a declarator's initializer a function or class is not
        // exported by the declaration around it.
        let exported = std::mem::take(&mut self.exported);
        walk::walk_variable_declaration(self, it);
        self.exported = exported;
    }
    fn visit_function(
        &mut self,
        it: &oxc_ast::ast::Function<'a>,
        flags: oxc_syntax::scope::ScopeFlags,
    ) {
        if it.is_declaration() {
            if let Some(id) = &it.id {
                self.push(id, it.span, false, false);
            }
        }
        let exported = std::mem::take(&mut self.exported);
        walk::walk_function(self, it, flags);
        self.exported = exported;
    }
    fn visit_class(&mut self, it: &oxc_ast::ast::Class<'a>) {
        if it.is_declaration() {
            if let Some(id) = &it.id {
                self.push(id, it.span, false, true);
            }
        }
        let exported = std::mem::take(&mut self.exported);
        walk::walk_class(self, it);
        self.exported = exported;
    }
    fn visit_for_of_statement(&mut self, it: &oxc_ast::ast::ForOfStatement<'a>) {
        if let oxc_ast::ast::ForStatementLeft::VariableDeclaration(v) = &it.left {
            self.heads.insert(v.span.start);
        }
        walk::walk_for_of_statement(self, it);
    }
    fn visit_for_in_statement(&mut self, it: &oxc_ast::ast::ForInStatement<'a>) {
        if let oxc_ast::ast::ForStatementLeft::VariableDeclaration(v) = &it.left {
            self.heads.insert(v.span.start);
        }
        walk::walk_for_in_statement(self, it);
    }
}

/// Every identifier reference's span, by reference id.
fn reference_spans(program: &Program) -> HashMap<oxc_syntax::reference::ReferenceId, Span> {
    use oxc_ast::ast::IdentifierReference;
    struct Spans(HashMap<oxc_syntax::reference::ReferenceId, Span>);
    impl<'a> Visit<'a> for Spans {
        fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
            if let Some(id) = it.reference_id.get() {
                self.0.insert(id, it.span);
            }
        }
    }
    let mut s = Spans(HashMap::new());
    s.visit_program(program);
    s.0
}

/// The names a binding pattern declares (Babel's `findVariables`).
fn pattern_names<'p, 'a>(pattern: &'p BindingPattern<'a>) -> Vec<&'p BindingIdentifier<'a>> {
    fn walk_p<'p, 'a>(p: &'p BindingPattern<'a>, out: &mut Vec<&'p BindingIdentifier<'a>>) {
        match p {
            BindingPattern::BindingIdentifier(id) => out.push(id),
            BindingPattern::ObjectPattern(o) => {
                for prop in &o.properties {
                    walk_p(&prop.value, out);
                }
                if let Some(rest) = &o.rest {
                    walk_p(&rest.argument, out);
                }
            }
            BindingPattern::ArrayPattern(a) => {
                for el in a.elements.iter().flatten() {
                    walk_p(el, out);
                }
                if let Some(rest) = &a.rest {
                    walk_p(&rest.argument, out);
                }
            }
            BindingPattern::AssignmentPattern(a) => walk_p(&a.left, out),
        }
    }
    let mut out = Vec::new();
    walk_p(pattern, &mut out);
    out
}

/// An edit: delete `start..end`, writing `with` (and the newlines it took).
#[derive(Clone)]
struct Edit {
    start: usize,
    end: usize,
    with: &'static str,
}

/// Nested candidate offsets carried through `edits` (sorted, disjoint): one
/// inside a deleted range is gone.
fn remap(offsets: &HashSet<u32>, edits: &[Applied]) -> HashSet<u32> {
    offsets
        .iter()
        .filter_map(|&at| {
            let at = at as usize;
            let mut shift: isize = 0;
            for &(start, end, written) in edits {
                if end <= at {
                    shift += written as isize - (end - start) as isize;
                } else if start <= at {
                    return None;
                } else {
                    break;
                }
            }
            Some((at as isize + shift) as u32)
        })
        .collect()
}

/// Deletions removing `removed` items of a comma-separated list (`items` in
/// order): an item before a kept one goes with what follows it up to the
/// next item; a trailing run goes from the last kept item's end.
fn list_deletions(items: &[Span], removed: &[bool]) -> Vec<(usize, usize)> {
    let last_kept = removed.iter().rposition(|r| !r);
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if !removed[i] {
            continue;
        }
        match last_kept {
            Some(k) if i < k => out.push((item.start as usize, items[i + 1].start as usize)),
            Some(k) => {
                if i == k + 1 {
                    out.push((items[k].end as usize, items.last().unwrap().end as usize));
                }
            }
            None => out.push((item.start as usize, item.end as usize)),
        }
    }
    out
}

/// Where an identifier sits in its pattern, for Babel's rest rule.
#[derive(Clone, Copy, PartialEq)]
enum Place {
    /// A declarator's id, an array element, a default's left side.
    Plain,
    /// An object property's value; `true` when the pattern ends in a rest.
    PropertyValue(bool),
    /// An object pattern's rest argument.
    Rest,
}

/// An applied edit: `(start, end, bytes written in its place)`.
type Applied = (usize, usize, usize);

/// One pass: the new code and its edits, or None when nothing is dead.
fn drop_pass(
    code: &str,
    path: &str,
    module: &HashSet<String>,
    nested: &HashSet<u32>,
) -> Option<(String, Vec<Applied>)> {
    let edits = Analysis::of(code, path, |a| {
        let mut walk = Removals {
            a,
            module,
            nested,
            lists: HashSet::new(),
            for_inits: HashSet::new(),
            heads: HashSet::new(),
            edits: Vec::new(),
        };
        walk.visit_program(a.program);
        walk.edits
    })?;
    if edits.is_empty() {
        return None;
    }
    let mut edits = edits;
    edits.sort_by_key(|e| (e.start, std::cmp::Reverse(e.end)));
    let mut out = String::with_capacity(code.len());
    let mut applied = Vec::new();
    let mut last = 0;
    for Edit { start, end, with } in edits {
        if start < last {
            continue;
        }
        out.push_str(&code[last..start]);
        let newlines = code[start..end].matches('\n').count();
        out.push_str(with);
        out.push_str(&"\n".repeat(newlines));
        applied.push((start, end, with.len() + newlines));
        last = end;
    }
    out.push_str(&code[last..]);
    (out != code).then_some((out, applied))
}

struct Removals<'r, 's, 'a> {
    a: &'r Analysis<'s, 'a>,
    module: &'r HashSet<String>,
    nested: &'r HashSet<u32>,
    /// Statements that are a statement list's items, by start.
    lists: HashSet<u32>,
    /// A `for` statement's init declaration, by start.
    for_inits: HashSet<u32>,
    /// `for…of` / `for…in` heads, by start.
    heads: HashSet<u32>,
    edits: Vec<Edit>,
}

impl Removals<'_, '_, '_> {
    /// Babel's `shouldBeRemoved`.
    fn dead(
        &self,
        id: &BindingIdentifier,
        place: Place,
        own_scope: Option<oxc_syntax::scope::ScopeId>,
    ) -> bool {
        let Some(decl) = self.a.decl_at(id.span.start) else {
            return false;
        };
        let candidate = if decl.module {
            self.module.contains(&decl.name)
        } else {
            self.nested.contains(&decl.at)
        };
        if !candidate || decl.exported || decl.is_class {
            return false;
        }
        // `ident.scope.getBinding(name)`: a function declaration's name is
        // looked up from inside the function, where a parameter or a local
        // of the same name answers.
        let symbol = own_scope
            .and_then(|scope| self.a.scoping.get_binding(scope, id.name))
            .unwrap_or(decl.symbol);
        if self.a.removable.contains(&symbol) {
            return true;
        }
        if self.a.read(symbol) || self.a.reassigned(symbol) || decl.loop_iter {
            return false;
        }
        match place {
            Place::Rest => true,
            Place::PropertyValue(rest_last) => !rest_last,
            Place::Plain => true,
        }
    }

    /// A declarator id's deletions and whether nothing of it is left.
    fn pattern(&self, p: &BindingPattern, place: Place, out: &mut Vec<(usize, usize)>) -> bool {
        match p {
            BindingPattern::BindingIdentifier(id) => self.dead(id, place, None),
            BindingPattern::AssignmentPattern(a) => match &a.left {
                // A default's identifier goes with its property (or element).
                BindingPattern::BindingIdentifier(id) => self.dead(id, Place::Plain, None),
                left => self.pattern(left, Place::Plain, out),
            },
            BindingPattern::ObjectPattern(o) => {
                let rest_last = o.rest.is_some();
                let mut items: Vec<Span> = o.properties.iter().map(|p| p.span).collect();
                let mut removed: Vec<bool> = o
                    .properties
                    .iter()
                    .map(|prop| {
                        let mut inner = Vec::new();
                        let gone =
                            self.pattern(&prop.value, Place::PropertyValue(rest_last), &mut inner);
                        if !gone {
                            out.extend(inner);
                        }
                        gone
                    })
                    .collect();
                if let Some(rest) = &o.rest {
                    items.push(rest.span);
                    let mut inner = Vec::new();
                    let gone = self.pattern(&rest.argument, Place::Rest, &mut inner);
                    if !gone {
                        out.extend(inner);
                    }
                    removed.push(gone);
                }
                if removed.iter().all(|r| *r) {
                    // Empty, or left empty: Babel's ObjectPattern visitor.
                    return true;
                }
                out.extend(list_deletions(&items, &removed));
                false
            }
            BindingPattern::ArrayPattern(arr) => {
                let mut any_left = false;
                for el in arr.elements.iter().flatten() {
                    let mut inner = Vec::new();
                    if self.pattern(el, Place::Plain, &mut inner) {
                        // A hole keeps its comma.
                        let s = el.span();
                        out.push((s.start as usize, s.end as usize));
                    } else {
                        out.extend(inner);
                        any_left = true;
                    }
                }
                if let Some(rest) = &arr.rest {
                    let mut inner = Vec::new();
                    if self.pattern(&rest.argument, Place::Plain, &mut inner) {
                        out.push((rest.span.start as usize, rest.span.end as usize));
                    } else {
                        out.extend(inner);
                        any_left = true;
                    }
                }
                // Empty, or every element a hole: Babel's ArrayPattern visitor.
                !any_left
            }
        }
    }

    fn remove_statement(&mut self, span: Span) {
        let with = if self.lists.contains(&span.start) || self.for_inits.contains(&span.start) {
            ""
        } else {
            // Not in a statement list (`if (x) var a = 1;`): an empty one.
            ";"
        };
        self.edits.push(Edit {
            start: span.start as usize,
            end: span.end as usize,
            with,
        });
    }
}

impl<'a> Visit<'a> for Removals<'_, '_, 'a> {
    fn visit_statements(&mut self, it: &oxc_allocator::Vec<'a, Statement<'a>>) {
        for s in it.iter() {
            self.lists.insert(s.span().start);
        }
        walk::walk_statements(self, it);
    }

    fn visit_import_declaration(&mut self, import: &oxc_ast::ast::ImportDeclaration<'a>) {
        if import.import_kind.is_type() {
            return;
        }
        let Some(specifiers) = &import.specifiers else {
            return;
        };
        let removed: Vec<bool> = specifiers
            .iter()
            .map(|s| self.dead(s.local(), Place::Plain, None))
            .collect();
        if !removed.iter().any(|r| *r) {
            return;
        }
        if removed.iter().all(|r| *r) {
            return self.remove_statement(import.span);
        }
        // `import D, * as N, { a, b } from`: the default, the namespace and
        // the braced named group are one comma list; the named ones are a
        // list of their own inside the braces.
        let code = self.a.code;
        let named: Vec<usize> = specifiers
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s, ImportDeclarationSpecifier::ImportSpecifier(_)))
            .map(|(i, _)| i)
            .collect();
        let mut clauses: Vec<Span> = Vec::new();
        let mut clause_removed: Vec<bool> = Vec::new();
        for (i, s) in specifiers.iter().enumerate() {
            if !named.contains(&i) {
                clauses.push(s.span());
                clause_removed.push(removed[i]);
            }
        }
        if let (Some(&first), Some(&last)) = (named.first(), named.last()) {
            let first_start = specifiers[first].span().start as usize;
            let last_end = specifiers[last].span().end as usize;
            let open = code[..first_start].rfind('{').unwrap_or(first_start);
            let close = code[last_end..]
                .find('}')
                .map_or(last_end, |c| last_end + c + 1);
            let group_gone = named.iter().all(|&i| removed[i]);
            clauses.push(Span::new(open as u32, close as u32));
            clause_removed.push(group_gone);
            if !group_gone {
                let spans: Vec<Span> = named.iter().map(|&i| specifiers[i].span()).collect();
                let flags: Vec<bool> = named.iter().map(|&i| removed[i]).collect();
                for (start, end) in list_deletions(&spans, &flags) {
                    self.edits.push(Edit {
                        start,
                        end,
                        with: "",
                    });
                }
            }
        }
        for (start, end) in list_deletions(&clauses, &clause_removed) {
            self.edits.push(Edit {
                start,
                end,
                with: "",
            });
        }
    }

    fn visit_variable_declaration(&mut self, var: &VariableDeclaration<'a>) {
        if self.heads.contains(&var.span.start) {
            return walk::walk_variable_declaration(self, var);
        }
        let mut removed = Vec::with_capacity(var.declarations.len());
        let mut inner = Vec::new();
        for d in &var.declarations {
            let mut parts = Vec::new();
            let gone = self.pattern(&d.id, Place::Plain, &mut parts);
            if !gone {
                inner.extend(parts);
            }
            removed.push(gone);
        }
        if removed.iter().all(|r| *r) && !var.declarations.is_empty() {
            return self.remove_statement(var.span);
        }
        let spans: Vec<Span> = var.declarations.iter().map(|d| d.span).collect();
        let mut edits: Vec<(usize, usize)> = list_deletions(&spans, &removed);
        edits.extend(inner);
        if edits.is_empty() {
            return walk::walk_variable_declaration(self, var);
        }
        for (start, end) in edits {
            self.edits.push(Edit {
                start,
                end,
                with: "",
            });
        }
        // What changes inside a kept declarator waits for the next pass.
    }

    fn visit_function(
        &mut self,
        f: &oxc_ast::ast::Function<'a>,
        flags: oxc_syntax::scope::ScopeFlags,
    ) {
        if f.is_declaration() {
            if let Some(id) = &f.id {
                if self.dead(id, Place::Plain, f.scope_id.get()) {
                    return self.remove_statement(f.span);
                }
            }
        }
        walk::walk_function(self, f, flags);
    }

    fn visit_for_statement(&mut self, it: &oxc_ast::ast::ForStatement<'a>) {
        if let Some(oxc_ast::ast::ForStatementInit::VariableDeclaration(v)) = &it.init {
            self.for_inits.insert(v.span.start);
        }
        walk::walk_for_statement(self, it);
    }
    fn visit_for_of_statement(&mut self, it: &oxc_ast::ast::ForOfStatement<'a>) {
        if let oxc_ast::ast::ForStatementLeft::VariableDeclaration(v) = &it.left {
            self.heads.insert(v.span.start);
        }
        walk::walk_for_of_statement(self, it);
    }
    fn visit_for_in_statement(&mut self, it: &oxc_ast::ast::ForInStatement<'a>) {
        if let oxc_ast::ast::ForStatementLeft::VariableDeclaration(v) = &it.left {
            self.heads.insert(v.span.start);
        }
        walk::walk_for_in_statement(self, it);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `before` with `remove` cut out (the edit), then the pass.
    fn dce(before: &str, remove: &str) -> String {
        assert!(before.contains(remove), "{remove} not in {before}");
        let after = before.replacen(remove, "", 1);
        let out = drop_dead_bindings(before, after, "src/x.ts");
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, &out, SourceType::ts()).parse();
        assert!(
            parsed.diagnostics.is_empty(),
            "{:?}\n{out}",
            parsed.diagnostics
        );
        assert_eq!(out.lines().count(), before.lines().count(), "lines:\n{out}");
        out
    }

    // The reviewer's cases, each with what babel-dead-code-elimination
    // makes of it after the same edit (Babel ran on each one).

    // A multi-line destructure only the removed code used goes whole, and a
    // later binding in the same function is still found: oj's own edits
    // never move lines or offsets out from under the candidates.
    #[test]
    fn babel_a_multi_line_destructure_goes_whole() {
        let before = "export function make() {\n  const {\n    a,\n    b,\n  } = pick();\n  const h = () => b;\n  return keep(() => [a, h()]);\n}\n";
        let out = dce(before, "() => [a, h()]");
        assert!(!out.contains("pick") && !out.contains("const h"), "{out}");
    }

    #[test]
    fn babel_a_multi_line_destructure_and_a_server_helper_go() {
        let before = "import { getRequest } from \"@tanstack/react-start/server\";\nexport function make() {\n  const {\n    a,\n    b,\n  } = pick();\n  const h = () => b;\n  const g = () => getRequest();\n  return [keep(() => [a, h(), g()])];\n}\n";
        let out = dce(before, "() => [a, h(), g()]");
        for gone in ["pick", "const h", "const g", "getRequest"] {
            assert!(!out.contains(gone), "{gone} left in:\n{out}");
        }
    }

    // A redeclared `var` is a constant violation: both declarations stay.
    #[test]
    fn babel_a_redeclared_var_stays() {
        let before =
            "export function make() {\n  var a = 1;\n  var a = 2;\n  return keep(() => a);\n}\n";
        let out = dce(before, "() => a");
        assert_eq!(out.matches("var a").count(), 2, "{out}");
    }

    // Only assigned, never read: never a candidate.
    #[test]
    fn babel_a_binding_only_assigned_stays() {
        let before =
            "export function make() {\n  let cached;\n  return keep(() => { cached = 1; });\n}\n";
        let out = dce(before, "() => { cached = 1; }");
        assert!(out.contains("let cached;"), "{out}");
    }

    // Two declarations of one name on one line are two candidates.
    #[test]
    fn babel_same_named_declarations_on_one_line_stay_apart() {
        let before = "export function make() { const a = g(); function k() { const a = side(); } return [k, keep(() => a)]; }\n";
        let out = dce(before, "() => a");
        assert!(!out.contains("g()"), "{out}");
        assert!(out.contains("const a = side();"), "{out}");
    }

    // A module-level `var` declared in a block is a module binding.
    #[test]
    fn babel_a_block_var_at_module_level_goes_with_its_import() {
        let before = "import { getRequest } from \"@tanstack/react-start/server\";\nif (globalThis.x) { var helper = getRequest; }\nexport const fn = keep(() => helper());\n";
        let out = dce(before, "() => helper()");
        assert!(
            !out.contains("helper =") && !out.contains("getRequest"),
            "{out}"
        );
        assert!(out.contains("if (globalThis.x) {"), "{out}");
    }

    // Babel's rest rule reaches only a property whose value is the name: a
    // defaulted one goes even with a rest after it.
    #[test]
    fn babel_a_defaulted_property_goes_before_a_rest() {
        let before = "export function make(obj: any) {\n  const { a = 1, ...rest } = obj;\n  return [rest, keep(() => a)];\n}\n";
        let out = dce(before, "() => a");
        assert!(
            out.contains("...rest } = obj;") && !out.contains("a = 1"),
            "{out}"
        );
    }

    // An empty pattern in a declarator goes, even one empty from the start.
    #[test]
    fn babel_an_empty_pattern_goes() {
        let before = "export function make(obj: any) {\n  const {} = obj;\n  const [] = obj;\n  return keep(() => 1);\n}\n";
        let out = dce(before, "() => 1");
        assert!(!out.contains("= obj"), "{out}");
    }

    // A class in a cycle with a helper: the helper (and its import) goes,
    // the class stays (Babel has no class visitor).
    #[test]
    fn babel_a_class_cycle_drops_the_helper_and_keeps_the_class() {
        let before = "import { getRequest } from \"@tanstack/react-start/server\";\nclass C { m() { return h(); } }\nfunction h() { getRequest(); return new C(); }\nexport const fn = keep(() => h());\n";
        let out = dce(before, "() => h()");
        assert!(
            !out.contains("function h") && !out.contains("getRequest"),
            "{out}"
        );
        assert!(out.contains("class C"), "{out}");
    }

    // Babel looks a function declaration's name up from inside it: a
    // parameter of the same name that is read keeps the function.
    #[test]
    fn babel_a_function_whose_parameter_shares_its_name_stays() {
        let before = "export function make() {\n  function req(req: number) { return req; }\n  return keep(() => req(1));\n}\n";
        let out = dce(before, "() => req(1)");
        assert!(out.contains("function req(req: number)"), "{out}");
    }

    #[test]
    fn babel_one_declarator_of_a_list_goes() {
        let before = "import { getRequest } from \"@tanstack/react-start/server\";\nconst a = getRequest,\n  b = 2;\nexport const fn = keep(() => a());\n";
        let out = dce(before, "() => a()");
        assert!(
            !out.contains("getRequest") && out.contains("b = 2;"),
            "{out}"
        );
    }

    // An export inside a namespace is an export.
    #[test]
    fn babel_a_namespace_export_stays() {
        let before =
            "export namespace N {\n  export const x = 1;\n  export const f = keep(() => x);\n}\n";
        let out = dce(before, "() => x");
        assert!(out.contains("export const x = 1;"), "{out}");
    }

    // A type reference reads the binding.
    #[test]
    fn babel_a_type_reference_keeps_its_binding() {
        let before = "export function make() {\n  const schema = mk();\n  type T = typeof schema;\n  let v: T | undefined;\n  return [v, keep(() => schema)];\n}\n";
        let out = dce(before, "() => schema");
        assert!(out.contains("const schema = mk();"), "{out}");
    }

    // A `for` head's declarator goes like any other; a loop iterator stays.
    #[test]
    fn babel_a_for_head_declarator_goes() {
        let before = "import { getRequest } from \"@tanstack/react-start/server\";\nexport function make(xs: number[]) {\n  for (let i = 0, req = getRequest; i < 1; i++) { keep(() => req()); }\n  for (const x of xs) keep(() => x);\n}\n";
        let out = dce(before, "() => req()");
        assert!(out.contains("for (let i = 0; i < 1; i++)"), "{out}");
        assert!(!out.contains("getRequest"), "{out}");
        assert!(out.contains("for (const x of xs)"), "{out}");
    }

    // On one line, the edit removes a read declaration of `a`; a later
    // declaration of `a` that nothing read is not taken for it (and its
    // side effect stays), as Babel's candidates are the nodes themselves.
    #[test]
    fn a_later_same_named_declaration_on_the_edited_line_is_not_a_candidate() {
        let before = "export function make() { const o = [keep(() => { const a = 1; return a; }), () => { const a = side2(); }]; return o; }\n";
        let out = dce(before, "() => { const a = 1; return a; }");
        assert!(out.contains("const a = side2();"), "{out}");
    }

    #[test]
    fn offsets_map_through_an_edit_on_their_line() {
        let before = "const x = 1; keep(() => handler()); const y = 2;\nnext();\n";
        let after = "const x = 1; keep(); const y = 2;\nnext();\n";
        let at = |code: &str, s: &str| code.find(s).unwrap() as u32;
        assert_eq!(
            map_offset(before, after, at(before, "x =")),
            Some(at(after, "x ="))
        );
        assert_eq!(
            map_offset(before, after, at(before, "y =")),
            Some(at(after, "y ="))
        );
        assert_eq!(map_offset(before, after, at(before, "handler")), None);
        assert_eq!(
            map_offset(before, after, at(before, "next")),
            Some(at(after, "next"))
        );
    }

    #[test]
    fn what_only_the_removed_code_used_goes_transitively() {
        let before = "import { getRequest } from \"@tanstack/react-start/server\";\n\
                      import { keep } from \"./keep\";\n\
                      const header = () => getRequest()?.headers.get(\"x\");\n\
                      function meta() { return header(); }\n\
                      export const a = keep(() => meta());\n";
        let out = dce(before, "() => meta()");
        for gone in ["getRequest", "header", "meta"] {
            assert!(!out.contains(gone), "{gone} left in:\n{out}");
        }
        assert!(out.contains("import { keep } from \"./keep\";"), "{out}");
    }

    // findRemovableBindings: a self-recursive helper the removed code was the
    // only user of goes, and the import only it used goes with it.
    #[test]
    fn a_self_recursive_helper_goes_with_its_import() {
        let before = "import { getRequest } from \"@tanstack/react-start/server\";\n\
                      function hdr(n: string, d = 0): string | null { return d > 2 ? null : getRequest()?.headers.get(n) ?? hdr(n, d + 1); }\n\
                      export const f = wrap(() => hdr(\"x\"));\n";
        let out = dce(before, "() => hdr(\"x\")");
        assert!(!out.contains("hdr") && !out.contains("getRequest"), "{out}");
    }

    #[test]
    fn helpers_that_only_call_each_other_go_together() {
        let before = "import { getRequest } from \"@tanstack/react-start/server\";\n\
                      const a = (n: number): unknown => (n ? b(n - 1) : null);\n\
                      const b = (n: number): unknown => a(n) ?? getRequest();\n\
                      export const f = wrap(() => a(3));\n";
        let out = dce(before, "() => a(3)");
        for gone in ["const a", "const b", "getRequest"] {
            assert!(!out.contains(gone), "{gone} left in:\n{out}");
        }
    }

    // A cycle something live still uses stays.
    #[test]
    fn a_cycle_a_live_binding_uses_stays() {
        let before = "const a = (n: number): unknown => (n ? b(n - 1) : null);\n\
                      const b = (n: number): unknown => a(n);\n\
                      export const live = () => b(1);\n\
                      export const f = wrap(() => a(3));\n";
        let out = dce(before, "() => a(3)");
        assert!(out.contains("const a") && out.contains("const b"), "{out}");
    }

    #[test]
    fn destructured_names_go_and_an_emptied_pattern_takes_its_declarator() {
        let before = "import * as S from \"@tanstack/react-start/server\";\n\
                      import { cfg } from \"./cfg\";\n\
                      const { getRequest } = S;\n\
                      const { host, port } = cfg;\n\
                      const [first, second] = list();\n\
                      export const f = wrap(() => getRequest() + port + first);\n\
                      export const g = host + second;\n";
        let out = dce(before, "() => getRequest() + port + first");
        assert!(!out.contains("getRequest") && !out.contains(" S;"), "{out}");
        assert!(!out.contains("@tanstack/react-start/server"), "{out}");
        assert!(out.contains("const { host } = cfg;"), "{out}");
        assert!(out.contains("const [, second] = list();"), "{out}");
    }

    // Babel's rest rule: with a rest element, the other properties stay (the
    // rest would gather them); a dead rest itself goes.
    #[test]
    fn a_rest_element_keeps_the_properties_before_it() {
        let before = "const { a, ...others } = obj;\n\
                      const { b, ...gone } = obj;\n\
                      export const f = wrap(() => a + gone.x);\n\
                      export const g = others + b;\n";
        let out = dce(before, "() => a + gone.x");
        assert!(out.contains("const { a, ...others } = obj;"), "{out}");
        assert!(out.contains("const { b } = obj;"), "{out}");
    }

    #[test]
    fn exports_reassigned_and_already_dead_bindings_stay() {
        let before = "import { side } from \"./side\";\n\
                      import \"./effect\";\n\
                      import { unusedFromStart } from \"./unused\";\n\
                      export function exported() { return 1; }\n\
                      let counter = 0;\n\
                      function bump() { counter = counter + 1; }\n\
                      function deadAlready() { return 2; }\n\
                      export const f = wrap(() => exported() + side() + counter);\n\
                      bump();\n";
        let out = dce(before, "() => exported() + side() + counter");
        assert!(out.contains("export function exported()"), "{out}");
        assert!(out.contains("let counter = 0;"), "reassigned: {out}");
        assert!(out.contains("import \"./effect\";"), "{out}");
        assert!(
            out.contains("unusedFromStart"),
            "dead before the edit: {out}"
        );
        assert!(
            out.contains("function deadAlready"),
            "dead before the edit: {out}"
        );
        assert!(!out.contains("import { side }"), "{out}");
    }

    // Babel's visitors reach every scope: a helper declared inside a factory
    // that only the removed code used goes, then the import only it used.
    #[test]
    fn a_nested_helper_only_the_removed_code_used_goes() {
        let before = "import { getRequest } from \"@tanstack/react-start/server\";\n\
                      export function makeFns() {\n\
                      \x20 const req = () => getRequest();\n\
                      \x20 function header() { return req(); }\n\
                      \x20 const kept = 1;\n\
                      \x20 return wrap(() => header(), kept);\n\
                      }\n";
        let out = dce(before, "() => header(), ");
        for gone in ["getRequest", "const req", "function header"] {
            assert!(!out.contains(gone), "{gone} left in:\n{out}");
        }
        assert!(out.contains("const kept = 1;"), "{out}");
    }

    #[test]
    fn nested_reassigned_loop_and_already_unused_bindings_stay() {
        let before = "export function f(xs: number[]) {\n\
                      \x20 let n = 0;\n\
                      \x20 n = n + 1;\n\
                      \x20 for (const x of xs) use(x);\n\
                      \x20 for (let i = 0; i < 2; i++) use(i);\n\
                      \x20 const unusedFromStart = 1;\n\
                      \x20 return wrap(() => n);\n\
                      }\n";
        let out = dce(before, "() => n");
        assert!(out.contains("let n = 0;"), "reassigned: {out}");
        assert!(out.contains("for (const x of xs)"), "{out}");
        assert!(out.contains("for (let i = 0;"), "{out}");
        assert!(out.contains("const unusedFromStart = 1;"), "{out}");
    }

    // A nested declaration that is a statement's whole body goes as an empty
    // statement, so the code still parses.
    #[test]
    fn a_nested_declaration_as_a_whole_body_becomes_an_empty_statement() {
        let before = "export function f(x: boolean) {\n\
                      \x20 if (x) var a = 1;\n\
                      \x20 return wrap(() => a);\n\
                      }\n";
        let out = dce(before, "() => a");
        assert!(out.contains("if (x) ;"), "{out}");
    }

    #[test]
    fn an_import_keeps_its_live_specifiers() {
        let before = "import def, { a, b as c } from \"./m\";\n\
                      export const f = wrap(() => a + def);\n\
                      export const g = c;\n";
        let out = dce(before, "() => a + def");
        assert!(out.contains("import { b as c } from \"./m\";"), "{out}");
    }
}
