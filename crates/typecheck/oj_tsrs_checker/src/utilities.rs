use crate::*;
use std::cmp::Ordering;
use std::fmt::Display;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

// Non-function declarations of utilities.go are hand-ported in utilities_types.rs.

// utilities.go:21
pub fn new_diagnostic_for_node(
    node: Option<P<Node>>,
    message: Option<&'static Message>,
    args: &[&dyn Display],
) -> P<Diagnostic> {
    let mut file: Option<P<SourceFile>> = None;
    let mut loc = TextRange::default();
    if let Some(node) = node {
        file = ast::get_source_file_of_node(node);
        loc = tsrs_scanner::get_error_range_for_node(file.unwrap(), node);
    }
    ast::new_diagnostic(file, loc, message.unwrap(), args)
}

// utilities.go:31
pub(crate) fn new_diagnostic_chain_for_node(
    chain: Option<P<Diagnostic>>,
    node: P<Node>,
    message: Option<&'static Message>,
    args: &[&dyn Display],
) -> P<Diagnostic> {
    if let Some(chain) = chain {
        return ast::new_diagnostic_chain(chain, message.unwrap(), args);
    }
    new_diagnostic_for_node(Some(node), message, args)
}

// utilities.go:55
pub(crate) fn has_override_modifier(node: P<Node>) -> bool {
    ast::has_syntactic_modifier(node, ModifierFlags::Override)
}

// utilities.go:59
pub(crate) fn has_async_modifier(node: P<Node>) -> bool {
    ast::has_syntactic_modifier(node, ModifierFlags::Async)
}

// utilities.go:63
pub(crate) fn get_selected_modifier_flags(node: P<Node>, flags: ModifierFlags) -> ModifierFlags {
    node.modifier_flags() & flags
}

// utilities.go:67
pub(crate) fn has_readonly_modifier(node: P<Node>) -> bool {
    ast::has_modifier(node, ModifierFlags::Readonly)
}

// utilities.go:71
pub(crate) fn is_static_private_identifier_property(s: P<Symbol>) -> bool {
    match s.value_declaration() {
        Some(vd) => ast::is_private_identifier_class_element_declaration(vd) && ast::is_static(vd),
        None => false,
    }
}

// utilities.go:89
pub(crate) fn get_assignment_target_kind(node: P<Node>) -> AssignmentKind {
    let Some(target) = ast::get_assignment_target(node) else {
        return AssignmentKind::None;
    };
    match target.kind() {
        Kind::BinaryExpression => {
            let binary_operator = target.as_binary_expression().operator_token.kind();
            if binary_operator == Kind::EqualsToken
                || ast::is_logical_or_coalescing_assignment_operator(binary_operator)
            {
                return AssignmentKind::Definite;
            }
            return AssignmentKind::Compound;
        }
        Kind::PrefixUnaryExpression | Kind::PostfixUnaryExpression => {
            return AssignmentKind::Compound;
        }
        Kind::ForInStatement | Kind::ForOfStatement => return AssignmentKind::Definite,
        _ => {}
    }
    panic!("Unhandled case in getAssignmentTargetKind")
}

// utilities.go:109
pub(crate) fn is_delete_target(node: P<Node>) -> bool {
    if !ast::is_access_expression(node) {
        return false;
    }
    let node = ast::walk_up_parenthesized_expressions(node.parent());
    matches!(node, Some(n) if n.kind() == Kind::DeleteExpression)
}

// utilities.go:117
pub(crate) fn is_in_compound_like_assignment(node: P<Node>) -> bool {
    match ast::get_assignment_target(node) {
        Some(target) => {
            ast::is_assignment_expression(target, true /*excludeCompoundAssignment*/)
                && is_compound_like_assignment(target)
        }
        None => false,
    }
}

// utilities.go:122
fn is_compound_like_assignment(assignment: P<Node>) -> bool {
    let right = ast::skip_parentheses(assignment.as_binary_expression().right());
    right.kind() == Kind::BinaryExpression
        && is_shift_operator_or_higher(right.as_binary_expression().operator_token.kind())
}

// utilities.go:127
pub(crate) fn is_const_type_reference(node: P<Node>) -> bool {
    ast::is_type_reference_node(node)
        && node.type_arguments().is_empty()
        && ast::is_identifier(node.as_type_reference_node().type_name)
        && node.as_type_reference_node().type_name.text() == "const"
}

// isConstTypeReferenceName reports whether node is the `const` type name of a `const`
// assertion (`x as const` / `<const>x`), which must not be resolved as a real name.
// utilities.go:133
pub(crate) fn is_const_type_reference_name(node: Option<P<Node>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    if !ast::is_identifier(node) {
        return false;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    if !is_const_type_reference(parent) {
        return false;
    }
    match parent.parent() {
        Some(grandparent) => ast::is_assertion_expression(grandparent),
        None => false,
    }
}

// isExportAssignmentExpressionName reports whether node is (the root entity name of) the
// expression of an `export =` / `export default` assignment. Referencing a namespace or
// type-only name there is legal, and checkExportAssignment decides whether it is an error,
// so checkIdentifier must not report a value-usage error for it.
// utilities.go:143
pub(crate) fn is_export_assignment_expression_name(node: Option<P<Node>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    let mut current = node;
    while let Some(parent) = current.parent() {
        if !ast::is_property_access_or_qualified_name(parent) {
            break;
        }
        current = parent;
    }
    match current.parent() {
        Some(parent) => ast::is_export_assignment(parent) && parent.expression() == Some(current),
        None => false,
    }
}

// utilities.go:161
pub(crate) fn is_type_reference_identifier(node: P<Node>) -> bool {
    let mut node = node;
    while node.parent().unwrap().kind() == Kind::QualifiedName {
        node = node.parent().unwrap();
    }
    ast::is_type_reference_node(node.parent().unwrap())
}

// utilities.go:168
pub fn is_in_type_query(node: P<Node>) -> bool {
    // TypeScript 1.0 spec (April 2014): 3.6.3
    // A type query consists of the keyword typeof followed by an expression.
    // The expression is restricted to a single identifier or a sequence of identifiers separated by periods
    ast::find_ancestor_or_quit(node, |n| match n.kind() {
        Kind::TypeQuery => FindAncestorResult::True,
        Kind::Identifier | Kind::QualifiedName => FindAncestorResult::False,
        _ => FindAncestorResult::Quit,
    })
    .is_some()
}

// utilities.go:183
pub(crate) fn can_have_locals(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::ArrowFunction
            | Kind::Block
            | Kind::CallSignature
            | Kind::CaseBlock
            | Kind::CatchClause
            | Kind::ClassStaticBlockDeclaration
            | Kind::ConditionalType
            | Kind::Constructor
            | Kind::ConstructorType
            | Kind::ConstructSignature
            | Kind::ForStatement
            | Kind::ForInStatement
            | Kind::ForOfStatement
            | Kind::FunctionDeclaration
            | Kind::FunctionExpression
            | Kind::FunctionType
            | Kind::GetAccessor
            | Kind::IndexSignature
            | Kind::JSDocSignature
            | Kind::MappedType
            | Kind::MethodDeclaration
            | Kind::MethodSignature
            | Kind::ModuleDeclaration
            | Kind::SetAccessor
            | Kind::SourceFile
            | Kind::TypeAliasDeclaration
            | Kind::JSTypeAliasDeclaration
    )
}

// utilities.go:197
pub(crate) fn is_shorthand_ambient_module_symbol(module_symbol: P<Symbol>) -> bool {
    is_shorthand_ambient_module(module_symbol.value_declaration())
}

// utilities.go:201
fn is_shorthand_ambient_module(node: Option<P<Node>>) -> bool {
    // The only kind of module that can be missing a body is a shorthand ambient module.
    matches!(node, Some(n) if n.kind() == Kind::ModuleDeclaration && n.body().is_none())
}

// utilities.go:206
pub(crate) fn get_alias_declaration_from_name(node: P<Node>) -> Option<P<Node>> {
    let parent = node.parent().unwrap();
    match parent.kind() {
        Kind::ImportClause
        | Kind::ImportSpecifier
        | Kind::NamespaceImport
        | Kind::ExportSpecifier
        | Kind::ExportAssignment
        | Kind::ImportEqualsDeclaration
        | Kind::NamespaceExport => Some(parent),
        Kind::QualifiedName => get_alias_declaration_from_name(parent),
        _ => None,
    }
}

// utilities.go:217
pub(crate) fn entity_name_to_string(name: P<Node>) -> String {
    ast::entity_name_to_string(name, Some(&tsrs_scanner::get_text_of_node))
}

// utilities.go:221
pub(crate) fn get_containing_qualified_name_node(node: P<Node>) -> P<Node> {
    let mut node = node;
    while ast::is_qualified_name(node.parent().unwrap()) {
        node = node.parent().unwrap();
    }
    node
}

// utilities.go:228
pub(crate) fn is_side_effect_import(node: P<Node>) -> bool {
    let ancestor = ast::find_ancestor(node, ast::is_import_declaration);
    matches!(ancestor, Some(a) if a.import_clause().is_none())
}

// utilities.go:233
pub(crate) fn get_external_module_require_argument(node: P<Node>) -> Option<P<Node>> {
    if ast::is_variable_declaration_initialized_to_require(node) {
        return Some(node.initializer().unwrap().arguments()[0]);
    }
    None
}

// utilities.go:240
pub(crate) fn is_right_side_of_access_expression(node: P<Node>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    ast::is_property_access_expression(parent) && parent.name() == Some(node)
        || ast::is_element_access_expression(parent)
            && parent.as_element_access_expression().argument_expression == node
}

// utilities.go:245
pub(crate) fn is_top_level_in_external_module_augmentation(node: Option<P<Node>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    let Some(parent) = node.parent() else {
        return false;
    };
    ast::is_module_block(parent) && ast::is_external_module_augmentation(parent.parent().unwrap())
}

// utilities.go:249
pub(crate) fn is_syntactic_default(node: P<Node>) -> bool {
    (ast::is_export_assignment(node) && !node.as_export_assignment().is_export_equals)
        || ast::has_syntactic_modifier(node, ModifierFlags::Default)
        || ast::is_export_specifier(node)
        || ast::is_namespace_export(node)
}

// utilities.go:256
pub(crate) fn has_export_assignment_symbol(module_symbol: P<Symbol>) -> bool {
    match module_symbol.exports() {
        Some(exports) => exports.lookup(INTERNAL_SYMBOL_NAME_EXPORT_EQUALS).is_some(),
        None => false,
    }
}

// utilities.go:260
pub(crate) fn is_type_alias(node: P<Node>) -> bool {
    ast::is_type_or_js_type_alias_declaration(node)
}

// utilities.go:264
pub(crate) fn has_only_expression_initializer(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::VariableDeclaration
            | Kind::Parameter
            | Kind::BindingElement
            | Kind::PropertyDeclaration
            | Kind::PropertyAssignment
            | Kind::EnumMember
    )
}

// utilities.go:272
pub(crate) fn has_dot_dot_dot_token(node: P<Node>) -> bool {
    match node.kind() {
        Kind::Parameter => node.as_parameter_declaration().dot_dot_dot_token().is_some(),
        Kind::BindingElement => node.as_binding_element().dot_dot_dot_token().is_some(),
        Kind::NamedTupleMember => node.as_named_tuple_member().dot_dot_dot_token.is_some(),
        Kind::JsxExpression => node.as_jsx_expression().dot_dot_dot_token.is_some(),
        _ => false,
    }
}

// utilities.go:286
pub fn is_type_any(t: Option<P<Type>>) -> bool {
    matches!(t, Some(t) if t.flags().intersects(TypeFlags::Any))
}

// utilities.go:294
pub(crate) fn is_exclamation_token(node: Option<P<Node>>) -> bool {
    matches!(node, Some(n) if n.kind() == Kind::ExclamationToken)
}

// utilities.go:298
pub(crate) fn is_optional_declaration(declaration: P<Node>) -> bool {
    ast::has_question_token(declaration)
}

impl Checker {
    // utilities.go:302
    pub(crate) fn is_optional_parameter(&mut self, node: P<Node>) -> bool {
        // !!! TODO: JSDoc support
        if ast::is_parameter_declaration(node) && node.question_token().is_some() {
            return true;
        }
        if !ast::is_parameter_declaration(node) {
            return false;
        }
        let parent = node.parent().unwrap();
        if node.initializer().is_some() {
            let signature = self.get_signature_from_declaration(parent);
            let parameter_index = parent
                .parameters()
                .iter()
                .position(|&p| p == node)
                .expect("parameter belongs to its declaration");
            // Only consider syntactic or instantiated parameters as optional, not `void` parameters as this function is used
            // in grammar checks and checking for `void` too early results in parameter types widening too early
            // and causes some noImplicitAny errors to be lost.
            return parameter_index
                >= self.get_min_argument_count_ex(
                    signature,
                    MinArgumentCountFlags::StrongArityForUntypedJS
                        | MinArgumentCountFlags::VoidIsNonOptional,
                );
        }
        let iife = ast::get_immediately_invoked_function_expression(parent);
        if let Some(iife) = iife {
            let parameter_index = parent
                .parameters()
                .iter()
                .position(|&p| p == node)
                .expect("parameter belongs to its declaration");
            return node.type_node().is_none()
                && node.as_parameter_declaration().dot_dot_dot_token().is_none()
                && parameter_index >= self.get_effective_call_arguments(iife).len();
        }
        false
    }
}

// utilities.go:329
pub(crate) fn is_empty_array_literal(expression: P<Node>) -> bool {
    ast::is_array_literal_expression(expression) && expression.elements().is_empty()
}

// utilities.go:333
pub(crate) fn declaration_belongs_to_private_ambient_member(declaration: P<Node>) -> bool {
    let root = ast::get_root_declaration(declaration);
    let mut member_declaration = root;
    if root.kind() == Kind::Parameter {
        member_declaration = root.parent().unwrap();
    }
    is_private_within_ambient(member_declaration)
}

// utilities.go:342
pub(crate) fn is_private_within_ambient(node: P<Node>) -> bool {
    (ast::has_modifier(node, ModifierFlags::Private)
        || ast::is_private_identifier_class_element_declaration(node))
        && node.flags().intersects(NodeFlags::Ambient)
}

// utilities.go:346
pub(crate) fn is_type_assertion(node: P<Node>) -> bool {
    ast::is_assertion_expression(ast::skip_parentheses(node))
}

// utilities.go:350
pub(crate) fn create_symbol_table(symbols: &[P<Symbol>]) -> Option<P<SymbolTable>> {
    if symbols.is_empty() {
        return None;
    }
    let result = SymbolTable::new();
    for &symbol in symbols {
        result.set(symbol.name(), symbol);
    }
    Some(result)
}

impl Checker {
    // utilities.go:361
    pub(crate) fn sort_symbols(&mut self, symbols: &mut [P<Symbol>]) {
        if self.program.source_files_complete() {
            if symbols.len() > 2 && self.sort_symbols_by_key(symbols) {
                return;
            }
            symbols.sort_by(|&a, &b| self.compare_symbols(Some(a), Some(b)));
        } else {
            // Not a total order (see `Program::source_files_complete`): Rust's sort would panic, Go's pdqsort sorts
            // anyway; its port gives Go's order.
            tsrs_core::goslices::sort_func(symbols, |&a, &b| {
                self.compare_symbols(Some(a), Some(b))
            });
        }
    }

    /// tsrs-only: `sort_symbols` with each symbol's place computed once instead of in every comparison. With every
    /// file of the program indexed, `compare_symbols` orders by (a first declaration, its file's index, its position,
    /// name, id), lexicographically. False (nothing sorted) when a declaration's file has no index.
    fn sort_symbols_by_key(&mut self, symbols: &mut [P<Symbol>]) -> bool {
        let mut keyed: Vec<(bool, usize, tsrs_core::TextPos, &'static str, P<Symbol>)> =
            Vec::with_capacity(symbols.len());
        for &symbol in symbols.iter() {
            let key = match symbol.declarations().first() {
                Some(&d) => {
                    let (file, index) = self.source_file_and_index_of_node(d);
                    // Index 0 is also what a file without one gets.
                    if file == 0
                        || index == 0
                            && ast::get_source_file_of_node(d)
                                .is_none_or(|s| !self.file_index_map.contains_key(&s))
                    {
                        return false;
                    }
                    (false, index, d.pos())
                }
                None => (true, 0, tsrs_core::TextPos::Synthetic),
            };
            keyed.push((key.0, key.1, key.2, symbol.name(), symbol));
        }
        // The id last, and only for a tie (getting it assigns one, as in `compare_symbols_worker`).
        keyed.sort_by(|a, b| {
            (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)).then_with(|| compare_names(a.3, b.3)).then_with(
                || {
                    if a.4 == b.4 {
                        return Ordering::Equal;
                    }
                    ast::get_symbol_id(a.4).0.cmp(&ast::get_symbol_id(b.4).0)
                },
            )
        });
        for (slot, k) in symbols.iter_mut().zip(keyed) {
            *slot = k.4;
        }
        true
    }

    // utilities.go:365
    pub(crate) fn compare_symbols_worker(
        &mut self,
        s1: Option<P<Symbol>>,
        s2: Option<P<Symbol>>,
    ) -> Ordering {
        if s1 == s2 {
            return Ordering::Equal;
        }
        let Some(s1) = s1 else {
            return Ordering::Greater;
        };
        let Some(s2) = s2 else {
            return Ordering::Less;
        };
        let d1 = s1.declarations().first().copied();
        let d2 = s2.declarations().first().copied();
        if d1.is_some() && d2.is_some() {
            let r = self.compare_nodes(d1, d2);
            if r != Ordering::Equal {
                return r;
            }
        } else if d1.is_some() {
            return Ordering::Less;
        } else if d2.is_some() {
            return Ordering::Greater;
        }
        let r = compare_names(s1.name(), s2.name());
        if r != Ordering::Equal {
            return r;
        }
        // Fall back to symbol IDs. This is a last resort that should happen only when symbols have
        // no declaration and duplicate names.
        ast::get_symbol_id(s1).cmp(&ast::get_symbol_id(s2))
    }

    // utilities.go:392
    pub(crate) fn compare_nodes(&mut self, n1: Option<P<Node>>, n2: Option<P<Node>>) -> Ordering {
        if n1 == n2 {
            return Ordering::Equal;
        }
        let Some(n1) = n1 else {
            return Ordering::Greater;
        };
        let Some(n2) = n2 else {
            return Ordering::Less;
        };
        // Two nodes with one parent are in one file (most comparisons sort the members of one declaration).
        if n1.parent().is_some()
            && n1.parent() == n2.parent()
            && n1.kind() != Kind::SourceFile
            && n2.kind() != Kind::SourceFile
        {
            return n1.pos().cmp(&n2.pos());
        }
        let (s1, f1) = self.source_file_and_index_of_node(n1);
        let (s2, f2) = self.source_file_and_index_of_node(n2);
        if s1 != s2 {
            // Order by index of file in the containing program
            return f1.cmp(&f2);
        }
        // In the same file, order by source position
        n1.pos().cmp(&n2.pos())
    }
}

impl Checker {
    /// `get_source_file_of_node(n)` (as a key, 0 for none) and its `file_index_map` index (0 for none), through a
    /// direct-mapped cache. tsrs-only: `compare_nodes` orders the declarations of union constituents, which walked
    /// both nodes' parents to their files and hashed the files for every comparison; a node's file never changes.
    #[inline]
    fn source_file_and_index_of_node(&mut self, n: P<Node>) -> (tsrs_core::ptr::PKey, usize) {
        const BITS: u32 = 12;
        let key = n.key();
        // The mask matters only with plain pointers, whose keys are 64-bit addresses.
        let slot = tsrs_core::ptr::key_to_usize(key.wrapping_mul(0x9E37_79B1) >> (32 - BITS))
            & ((1 << BITS) - 1);
        if self.node_file_cache.is_empty() {
            self.node_file_cache = vec![(0, 0, 0); 1 << BITS];
        }
        let (k, file, index) = self.node_file_cache[slot];
        if k == key {
            return (file, index);
        }
        let s = ast::get_source_file_of_node(n);
        let file = s.map_or(0, |s| s.as_node().key());
        let index = s.and_then(|s| self.file_index_map.get(&s).copied()).unwrap_or(0);
        self.node_file_cache[slot] = (key, file, index);
        (file, index)
    }
}

// Go cmp.Compare for float64: NaN sorts before every other value and equals itself.
fn compare_numbers(a: jsnum::Number, b: jsnum::Number) -> Ordering {
    let (x, y) = (a.0, b.0);
    let x_nan = x.is_nan();
    let y_nan = y.is_nan();
    if x_nan {
        if y_nan {
            return Ordering::Equal;
        }
        return Ordering::Less;
    }
    if y_nan {
        return Ordering::Greater;
    }
    if x < y {
        return Ordering::Less;
    }
    if x > y {
        return Ordering::Greater;
    }
    Ordering::Equal
}

// utilities.go:414
/// The identity and flag-order answers inline (union construction sorts and binary-searches with this: on large
/// projects it runs >100M times, mostly decided by the flags); the rest out of line.
#[inline]
pub(crate) fn compare_types(c: &mut Checker, t1: Option<P<Type>>, t2: Option<P<Type>>) -> Ordering {
    if t1 == t2 {
        return Ordering::Equal;
    }
    let Some(t1) = t1 else {
        return Ordering::Less;
    };
    let Some(t2) = t2 else {
        return Ordering::Greater;
    };
    // First sort in order of increasing type flags values.
    let r = get_sort_order_flags(t1).cmp(&get_sort_order_flags(t2));
    if r != Ordering::Equal {
        return r;
    }
    // Two string literals without aliases (no name symbol): by value, then id, as compare_types_same_flags orders
    // them. Large unions of string literals are built by binary insertion with this.
    if t1.flags() == TypeFlags::StringLiteral && t1.alias().is_none() && t2.alias().is_none() {
        return compare_string_literal_types(t1, t2);
    }
    compare_types_same_flags(c, t1, t2)
}

#[inline(never)]
fn compare_string_literal_types(t1: P<Type>, t2: P<Type>) -> Ordering {
    literal_string_value(t1).cmp(literal_string_value(t2)).then_with(|| t1.id().0.cmp(&t2.id().0))
}

#[inline(never)]
fn compare_types_same_flags(c: &mut Checker, t1: P<Type>, t2: P<Type>) -> Ordering {
    // Order named types by name and, in the case of aliased types, by alias type arguments.
    let r = compare_type_names(c, t1, t2);
    if r != Ordering::Equal {
        return r;
    }
    // We have unnamed types or types with identical names. Now sort by data specific to the type.
    let flags = t1.flags();
    if flags.intersects(
        TypeFlags::Any
            | TypeFlags::Unknown
            | TypeFlags::String
            | TypeFlags::Number
            | TypeFlags::Boolean
            | TypeFlags::BigInt
            | TypeFlags::ESSymbol
            | TypeFlags::Void
            | TypeFlags::Undefined
            | TypeFlags::Null
            | TypeFlags::Never
            | TypeFlags::NonPrimitive,
    ) {
        // Only distinguished by type IDs, handled below.
    } else if flags.intersects(TypeFlags::Object) {
        // Order instantiation expression types without relying on lazy symbol IDs.
        // Order other unnamed or identically named object types by symbol.
        if t1.object_flags().intersects(ObjectFlags::InstantiationExpressionType)
            && t2.object_flags().intersects(ObjectFlags::InstantiationExpressionType)
        {
            let declaration1 = t1.symbol().and_then(|s| s.declarations().first().copied());
            let declaration2 = t2.symbol().and_then(|s| s.declarations().first().copied());
            // A single instantiation expression can produce multiple types for union constituents,
            // so compare their source declarations before comparing the shared expression node.
            let r = c.compare_nodes(declaration1, declaration2);
            if r != Ordering::Equal {
                return r;
            }
            let r = c.compare_nodes(
                t1.as_instantiation_expression_type().node.get(),
                t2.as_instantiation_expression_type().node.get(),
            );
            if r != Ordering::Equal {
                return r;
            }
        } else {
            let r = c.compare_symbols(t1.symbol(), t2.symbol());
            if r != Ordering::Equal {
                return r;
            }
        }
        // When object types have the same or no symbol, order by kind. We order type references before other kinds.
        if t1.object_flags().intersects(ObjectFlags::Reference)
            && t2.object_flags().intersects(ObjectFlags::Reference)
        {
            let r1 = t1.as_type_reference();
            let r2 = t2.as_type_reference();
            let r1_target = r1.target.get().unwrap();
            let r2_target = r2.target.get().unwrap();
            if r1_target.object_flags().intersects(ObjectFlags::Tuple)
                && r2_target.object_flags().intersects(ObjectFlags::Tuple)
            {
                // Tuple types have no associated symbol, instead we order by tuple element information.
                let r = compare_tuple_types(r1_target.as_tuple_type(), r2_target.as_tuple_type());
                if r != Ordering::Equal {
                    return r;
                }
            }
            // Here we know we have references to instantiations of the same type because we have matching targets.
            if r1.node.get().is_none() && r2.node.get().is_none() {
                // Non-deferred type references with the same target are sorted by their type argument lists.
                let r = compare_type_lists(
                    c,
                    t1.as_type_reference().resolved_type_arguments.get().unwrap_or(&[]),
                    t2.as_type_reference().resolved_type_arguments.get().unwrap_or(&[]),
                );
                if r != Ordering::Equal {
                    return r;
                }
            } else {
                // Deferred type references with the same target are ordered by the source location of the reference.
                let r = c.compare_nodes(r1.node.get(), r2.node.get());
                if r != Ordering::Equal {
                    return r;
                }
                // Instantiations of the same deferred type reference are ordered by their associated type mappers
                // (which reflect the mapping of in-scope type parameters to type arguments).
                let r = compare_type_mappers(
                    c,
                    t1.as_object_type().mapper.get(),
                    t2.as_object_type().mapper.get(),
                );
                if r != Ordering::Equal {
                    return r;
                }
            }
        } else if t1.object_flags().intersects(ObjectFlags::Reference) {
            return Ordering::Less;
        } else if t2.object_flags().intersects(ObjectFlags::Reference) {
            return Ordering::Greater;
        } else {
            // Order unnamed non-reference object types by kind and instantiation data.
            let r = (t1.object_flags() & ObjectFlags::ObjectTypeKindMask)
                .bits()
                .cmp(&(t2.object_flags() & ObjectFlags::ObjectTypeKindMask).bits());
            if r != Ordering::Equal {
                return r;
            }
            if t1.object_flags().intersects(ObjectFlags::ReverseMapped) {
                let r1 = t1.as_reverse_mapped_type();
                let r2 = t2.as_reverse_mapped_type();
                let r = compare_types(c, r1.source.get(), r2.source.get());
                if r != Ordering::Equal {
                    return r;
                }
                let r = compare_types(c, r1.mapped_type.get(), r2.mapped_type.get());
                if r != Ordering::Equal {
                    return r;
                }
                let r = compare_types(c, r1.constraint_type.get(), r2.constraint_type.get());
                if r != Ordering::Equal {
                    return r;
                }
            }
            let mut m1 = t1.as_object_type().mapper.get();
            let mut m2 = t2.as_object_type().mapper.get();
            if t1.object_flags().intersects(ObjectFlags::Mapped) {
                // instantiateAnonymousType prepends a fresh type parameter mapping.
                // Compare the effective instantiation, not the identity of that fresh parameter.
                if let Some(m) = m1 {
                    m1 = Some(composite_mapper_m2(m));
                }
                if let Some(m) = m2 {
                    m2 = Some(composite_mapper_m2(m));
                }
            }
            let r = compare_type_mappers(c, m1, m2);
            if r != Ordering::Equal {
                return r;
            }
        }
    } else if flags.intersects(TypeFlags::Union) {
        // Unions are ordered by origin and then constituent type lists.
        let o1 = t1.as_union_type().origin();
        let o2 = t2.as_union_type().origin();
        if o1.is_none() && o2.is_none() {
            let r = compare_type_lists(c, t1.types(), t2.types());
            if r != Ordering::Equal {
                return r;
            }
        } else if o1.is_none() {
            return Ordering::Greater;
        } else if o2.is_none() {
            return Ordering::Less;
        } else {
            let r = compare_types(c, o1, o2);
            if r != Ordering::Equal {
                return r;
            }
        }
    } else if flags.intersects(TypeFlags::Intersection) {
        // Intersections are ordered by their constituent type lists.
        let r = compare_type_lists(c, t1.types(), t2.types());
        if r != Ordering::Equal {
            return r;
        }
    } else if flags.intersects(TypeFlags::Enum | TypeFlags::EnumLiteral | TypeFlags::UniqueESSymbol)
    {
        // Enum members are ordered by their symbol (and thus their declaration order).
        let r = c.compare_symbols_worker(t1.symbol(), t2.symbol());
        if r != Ordering::Equal {
            return r;
        }
    } else if flags.intersects(TypeFlags::StringLiteral) {
        // String literal types are ordered by their values.
        let r = literal_string_value(t1).cmp(literal_string_value(t2));
        if r != Ordering::Equal {
            return r;
        }
    } else if flags.intersects(TypeFlags::NumberLiteral) {
        // Numeric literal types are ordered by their values.
        let r = compare_numbers(literal_number_value(t1), literal_number_value(t2));
        if r != Ordering::Equal {
            return r;
        }
    } else if flags.intersects(TypeFlags::BigIntLiteral) {
        let r = get_big_int_literal_value(t1).compare(get_big_int_literal_value(t2));
        if r != Ordering::Equal {
            return r;
        }
    } else if flags.intersects(TypeFlags::BooleanLiteral) {
        let b1 = literal_bool_value(t1);
        let b2 = literal_bool_value(t2);
        if b1 != b2 {
            if b1 {
                return Ordering::Greater;
            }
            return Ordering::Less;
        }
    } else if flags.intersects(TypeFlags::TypeParameter) {
        let r = c.compare_symbols_worker(t1.symbol(), t2.symbol());
        if r != Ordering::Equal {
            return r;
        }
    } else if flags.intersects(TypeFlags::Index) {
        let r = compare_types(c, t1.as_index_type().target.get(), t2.as_index_type().target.get());
        if r != Ordering::Equal {
            return r;
        }
        let r = t1
            .as_index_type()
            .index_flags
            .get()
            .bits()
            .cmp(&t2.as_index_type().index_flags.get().bits());
        if r != Ordering::Equal {
            return r;
        }
    } else if flags.intersects(TypeFlags::IndexedAccess) {
        let r = compare_types(
            c,
            t1.as_indexed_access_type().object_type.get(),
            t2.as_indexed_access_type().object_type.get(),
        );
        if r != Ordering::Equal {
            return r;
        }
        let r = compare_types(
            c,
            t1.as_indexed_access_type().index_type.get(),
            t2.as_indexed_access_type().index_type.get(),
        );
        if r != Ordering::Equal {
            return r;
        }
    } else if flags.intersects(TypeFlags::Conditional) {
        let r = c.compare_nodes(
            t1.as_conditional_type().root.get().unwrap().node.get(),
            t2.as_conditional_type().root.get().unwrap().node.get(),
        );
        if r != Ordering::Equal {
            return r;
        }
        let r = compare_type_mappers(
            c,
            t1.as_conditional_type().mapper.get(),
            t2.as_conditional_type().mapper.get(),
        );
        if r != Ordering::Equal {
            return r;
        }
    } else if flags.intersects(TypeFlags::Substitution) {
        let r = compare_types(
            c,
            t1.as_substitution_type().base_type.get(),
            t2.as_substitution_type().base_type.get(),
        );
        if r != Ordering::Equal {
            return r;
        }
        let r = compare_types(
            c,
            t1.as_substitution_type().constraint.get(),
            t2.as_substitution_type().constraint.get(),
        );
        if r != Ordering::Equal {
            return r;
        }
    } else if flags.intersects(TypeFlags::TemplateLiteral) {
        let r = t1
            .as_template_literal_type()
            .texts
            .get()
            .cmp(t2.as_template_literal_type().texts.get());
        if r != Ordering::Equal {
            return r;
        }
        let r = compare_type_lists(
            c,
            t1.as_template_literal_type().types.get(),
            t2.as_template_literal_type().types.get(),
        );
        if r != Ordering::Equal {
            return r;
        }
    } else if flags.intersects(TypeFlags::StringMapping) {
        let r = compare_types(
            c,
            t1.as_string_mapping_type().target.get(),
            t2.as_string_mapping_type().target.get(),
        );
        if r != Ordering::Equal {
            return r;
        }
    }
    // Fall back to type IDs. This results in type creation order for built-in types.
    t1.id().cmp(&t2.id())
}

// Go `m.data.(*CompositeTypeMapper).m2` (panics like the Go type assertion on any other mapper kind).
fn composite_mapper_m2(m: P<TypeMapper>) -> P<TypeMapper> {
    match m.data() {
        TypeMapperData::Composite { m2, .. } => m2,
        _ => panic!("interface conversion: expected *CompositeTypeMapper"),
    }
}

// Go `t.AsLiteralType().value.(string)` and friends.
fn literal_string_value(t: P<Type>) -> &'static str {
    match t.as_literal_type().value.get() {
        Some(LiteralValue::String(s)) => s,
        _ => panic!("interface conversion: literal value is not a string"),
    }
}

fn literal_number_value(t: P<Type>) -> jsnum::Number {
    match t.as_literal_type().value.get() {
        Some(LiteralValue::Number(n)) => n,
        _ => panic!("interface conversion: literal value is not a jsnum.Number"),
    }
}

fn literal_bool_value(t: P<Type>) -> bool {
    match t.as_literal_type().value.get() {
        Some(LiteralValue::Boolean(b)) => b,
        _ => panic!("interface conversion: literal value is not a bool"),
    }
}

// utilities.go:624
fn get_sort_order_flags(t: P<Type>) -> u32 {
    // Return TypeFlagsEnum for all enum-like unit types (they'll be sorted by their symbols)
    if t.flags().intersects(TypeFlags::EnumLiteral | TypeFlags::Enum)
        && !t.flags().intersects(TypeFlags::Union)
    {
        return TypeFlags::Enum.bits();
    }
    t.flags().bits()
}

// utilities.go:632
fn compare_type_names(c: &mut Checker, t1: P<Type>, t2: P<Type>) -> Ordering {
    let s1 = get_type_name_symbol(t1);
    let s2 = get_type_name_symbol(t2);
    if s1 == s2 {
        // The same alias (or none) has the same arguments (can1357's a3f430da0).
        if t1.alias() == t2.alias() {
            return Ordering::Equal;
        }
        return compare_type_lists(c, t1.alias().type_arguments(), t2.alias().type_arguments());
    }
    let Some(s1) = s1 else {
        return Ordering::Greater;
    };
    let Some(s2) = s2 else {
        return Ordering::Less;
    };
    let r = compare_names(s1.name(), s2.name());
    if r != Ordering::Equal {
        return r;
    }
    // Keep distinct same-named declarations together before comparing alias arguments or structure.
    c.compare_symbols(Some(s1), Some(s2))
}

/// `a.cmp(b)` as Go's `strings.Compare`, without reading the bytes when both are the same string (distinct symbols
/// often share their declaration's name string).
#[inline]
fn compare_names(a: &str, b: &str) -> Ordering {
    if a.len() == b.len() && std::ptr::eq(a.as_ptr(), b.as_ptr()) {
        return Ordering::Equal;
    }
    a.cmp(b)
}

// utilities.go:651
fn get_type_name_symbol(t: P<Type>) -> Option<P<Symbol>> {
    if let Some(alias) = t.alias() {
        return alias.symbol();
    }
    if t.flags().intersects(TypeFlags::TypeParameter | TypeFlags::StringMapping)
        || t.object_flags().intersects(ObjectFlags::ClassOrInterface | ObjectFlags::Reference)
    {
        return t.symbol();
    }
    None
}

// utilities.go:668
fn compare_tuple_types(t1: &'static TupleType, t2: &'static TupleType) -> Ordering {
    if std::ptr::eq(t1, t2) {
        return Ordering::Equal;
    }
    if t1.readonly.get() != t2.readonly.get() {
        return if t1.readonly.get() { Ordering::Greater } else { Ordering::Less };
    }
    let e1 = t1.element_infos.get();
    let e2 = t2.element_infos.get();
    if e1.len() != e2.len() {
        return e1.len().cmp(&e2.len());
    }
    for (i, element) in e1.iter().enumerate() {
        let r = element.flags.bits().cmp(&e2[i].flags.bits());
        if r != Ordering::Equal {
            return r;
        }
    }
    for (i, element) in e1.iter().enumerate() {
        let r = compare_element_labels(element.labeled_declaration, e2[i].labeled_declaration);
        if r != Ordering::Equal {
            return r;
        }
    }
    Ordering::Equal
}

// utilities.go:691
fn compare_element_labels(n1: Option<P<Node>>, n2: Option<P<Node>>) -> Ordering {
    if n1 == n2 {
        return Ordering::Equal;
    }
    let Some(n1) = n1 else {
        return Ordering::Less;
    };
    let Some(n2) = n2 else {
        return Ordering::Greater;
    };
    n1.name().unwrap().text().cmp(n2.name().unwrap().text())
}

// utilities.go:704
fn compare_type_lists(c: &mut Checker, s1: &[P<Type>], s2: &[P<Type>]) -> Ordering {
    if s1.len() != s2.len() {
        return s1.len().cmp(&s2.len());
    }
    if let Some(result) = s1.iter().enumerate().find_map(|(i, &t1)| {
        let r = compare_types(c, Some(t1), Some(s2[i]));
        if r != Ordering::Equal {
            return Some(r);
        }

        None
    }) {
        return result;
    }
    Ordering::Equal
}

// utilities.go:716
fn compare_type_mappers(
    c: &mut Checker,
    m1: Option<P<TypeMapper>>,
    m2: Option<P<TypeMapper>>,
) -> Ordering {
    if m1 == m2 {
        return Ordering::Equal;
    }
    let Some(m1) = m1 else {
        return Ordering::Greater;
    };
    let Some(m2) = m2 else {
        return Ordering::Less;
    };
    let kind1 = m1.kind();
    let kind2 = m2.kind();
    if kind1 != kind2 {
        return kind1.cmp(&kind2);
    }
    match (m1.data(), m2.data()) {
        (
            TypeMapperData::Simple { source: source1, target: target1 },
            TypeMapperData::Simple { source: source2, target: target2 },
        ) => {
            let r = compare_types(c, Some(source1), Some(source2));
            if r != Ordering::Equal {
                return r;
            }
            compare_types(c, Some(target1), Some(target2))
        }
        (d1, d2) if kind1 == TypeMapperKind::Array => {
            let ((sources1, targets1), (sources2, targets2)) =
                (d1.array_sources_targets().unwrap(), d2.array_sources_targets().unwrap());
            let r = compare_type_lists(c, sources1, sources2);
            if r != Ordering::Equal {
                return r;
            }
            compare_type_lists(c, targets1, targets2)
        }
        (
            TypeMapperData::Merged { m1: m11, m2: m12 },
            TypeMapperData::Merged { m1: m21, m2: m22 },
        ) => {
            let r = compare_type_mappers(c, Some(m11), Some(m21));
            if r != Ordering::Equal {
                return r;
            }
            compare_type_mappers(c, Some(m12), Some(m22))
        }
        _ => Ordering::Equal,
    }
}

// utilities.go:757
pub(crate) fn get_declaration_modifier_flags_from_symbol(s: P<Symbol>) -> ModifierFlags {
    get_declaration_modifier_flags_from_symbol_ex(s, false /*isWrite*/)
}

/// The `Private` and `Protected` bits of `get_declaration_modifier_flags_from_symbol(s)`. A declared symbol whose parent
/// is not a class has neither (its accessibility modifiers are dropped below), so its declaration is not read.
#[inline]
pub(crate) fn get_declaration_non_public_flags_from_symbol(s: P<Symbol>) -> ModifierFlags {
    if !s.check_flags().intersects(CheckFlags::Synthetic)
        && !s.parent().is_some_and(|p| p.flags().intersects(SymbolFlags::Class))
    {
        return ModifierFlags::None;
    }
    get_declaration_modifier_flags_from_symbol(s) & ModifierFlags::NonPublicAccessibilityModifier
}

// utilities.go:761
pub(crate) fn get_declaration_modifier_flags_from_symbol_ex(
    s: P<Symbol>,
    is_write: bool,
) -> ModifierFlags {
    let check_flags = s.check_flags();
    if check_flags.intersects(CheckFlags::Synthetic) {
        let mut access_modifier = ModifierFlags::None;
        if !is_write && check_flags.intersects(CheckFlags::ContainsPublic)
            || is_write && check_flags.intersects(CheckFlags::ContainsWritePublic)
        {
            access_modifier = ModifierFlags::Public;
        } else if !is_write && check_flags.intersects(CheckFlags::ContainsProtected)
            || is_write && check_flags.intersects(CheckFlags::ContainsWriteProtected)
        {
            access_modifier = ModifierFlags::Protected;
        } else if !is_write && check_flags.intersects(CheckFlags::ContainsPrivate)
            || is_write && check_flags.intersects(CheckFlags::ContainsWritePrivate)
        {
            access_modifier = ModifierFlags::Private;
        }
        if check_flags.intersects(CheckFlags::ContainsStatic) {
            return access_modifier | ModifierFlags::Static;
        }
        return access_modifier;
    }
    if let Some(value_declaration) = s.value_declaration() {
        let mut declaration: Option<P<Node>> = None;
        if is_write {
            declaration =
                s.declarations().iter().copied().find(|&d| ast::is_set_accessor_declaration(d));
        }
        if declaration.is_none() && s.flags().intersects(SymbolFlags::GetAccessor) {
            declaration =
                s.declarations().iter().copied().find(|&d| ast::is_get_accessor_declaration(d));
        }
        let declaration = declaration.unwrap_or(value_declaration);
        let flags = ast::get_combined_modifier_flags(declaration);
        if let Some(parent) = s.parent()
            && parent.flags().intersects(SymbolFlags::Class)
        {
            return flags;
        }
        return flags & !ModifierFlags::AccessibilityModifier;
    }
    if s.flags().intersects(SymbolFlags::Prototype) {
        return ModifierFlags::Public | ModifierFlags::Static;
    }
    ModifierFlags::None
}

// utilities.go:800
fn is_exponentiation_operator(kind: Kind) -> bool {
    kind == Kind::AsteriskAsteriskToken
}

// utilities.go:804
fn is_multiplicative_operator(kind: Kind) -> bool {
    kind == Kind::AsteriskToken || kind == Kind::SlashToken || kind == Kind::PercentToken
}

// utilities.go:808
fn is_multiplicative_operator_or_higher(kind: Kind) -> bool {
    is_exponentiation_operator(kind) || is_multiplicative_operator(kind)
}

// utilities.go:812
fn is_additive_operator(kind: Kind) -> bool {
    kind == Kind::PlusToken || kind == Kind::MinusToken
}

// utilities.go:816
fn is_additive_operator_or_higher(kind: Kind) -> bool {
    is_additive_operator(kind) || is_multiplicative_operator_or_higher(kind)
}

// utilities.go:820
fn is_shift_operator(kind: Kind) -> bool {
    kind == Kind::LessThanLessThanToken
        || kind == Kind::GreaterThanGreaterThanToken
        || kind == Kind::GreaterThanGreaterThanGreaterThanToken
}

// utilities.go:825
fn is_shift_operator_or_higher(kind: Kind) -> bool {
    is_shift_operator(kind) || is_additive_operator_or_higher(kind)
}

// utilities.go:867
pub(crate) fn is_object_literal_type(t: P<Type>) -> bool {
    t.object_flags().intersects(ObjectFlags::ObjectLiteral)
}

// utilities.go:871
pub(crate) fn is_declaration_readonly(declaration: P<Node>) -> bool {
    ast::get_combined_modifier_flags(declaration).intersects(ModifierFlags::Readonly)
        && !ast::is_parameter_property_declaration(declaration, declaration.parent().unwrap())
}

impl<T: Copy + Eq + std::hash::Hash> CheckerOrderedSet<T> {
    // utilities.go:884
    pub(crate) fn contains(&self, value: T) -> bool {
        match &self.values_by_key {
            None => self.values.contains(&value),
            Some(values_by_key) => values_by_key.contains(&value),
        }
    }

    // utilities.go:892
    pub(crate) fn add(&mut self, value: T) {
        self.values.push(value);
        // Small sets are served by a linear scan over values; only materialize the map once the set
        // grows large enough for hashing to win.
        if self.values_by_key.is_none() {
            if self.values.len() <= ORDERED_SET_MAP_THRESHOLD {
                return;
            }
            let mut m =
                FxHashSet::with_capacity_and_hasher(self.values.len(), rustc_hash::FxBuildHasher);
            m.extend(self.values[..self.values.len() - 1].iter().copied());
            self.values_by_key = Some(m);
        }
        self.values_by_key.as_mut().unwrap().insert(value);
    }
}

// utilities.go:908
pub(crate) fn get_containing_function_or_class_static_block(node: P<Node>) -> Option<P<Node>> {
    ast::find_ancestor(node.parent(), ast::is_function_like_or_class_static_block_declaration)
}

// utilities.go:912
pub(crate) fn is_node_descendant_of(node: Option<P<Node>>, ancestor: P<Node>) -> bool {
    let mut node = node;
    while let Some(n) = node {
        if n == ancestor {
            return true;
        }
        node = n.parent();
    }
    false
}

// utilities.go:922
pub(crate) fn is_type_usable_as_property_name(t: P<Type>) -> bool {
    t.flags().intersects(TypeFlags::StringOrNumberLiteralOrUnique)
}

// Gets the symbolic name for a member from its type.
// utilities.go:929
pub(crate) fn get_property_name_from_type(t: P<Type>) -> std::borrow::Cow<'static, str> {
    if t.flags().intersects(TypeFlags::StringLiteral) {
        return literal_string_value(t).into();
    }
    if t.flags().intersects(TypeFlags::NumberLiteral) {
        return literal_number_value(t).string().into();
    }
    if t.flags().intersects(TypeFlags::UniqueESSymbol) {
        return t.as_unique_es_symbol_type().name.get().into();
    }
    panic!("Unhandled case in getPropertyNameFromType")
}

// utilities.go:941
pub(crate) fn is_numeric_literal_name(name: &str) -> bool {
    // The intent of numeric names is that
    //     - they are names with text in a numeric form, and that
    //     - setting properties/indexing with them is always equivalent to doing so with the numeric literal 'numLit',
    //         acquired by applying the abstract 'ToNumber' operation on the name's text.
    //
    // The subtlety is in the latter portion, as we cannot reliably say that anything that looks like a numeric literal is a numeric name.
    // In fact, it is the case that the text of the name must be equal to 'ToString(numLit)' for this to hold.
    //
    // Consider the property name '"0xF00D"'. When one indexes with '0xF00D', they are actually indexing with the value of 'ToString(0xF00D)'
    // according to the ECMAScript specification, so it is actually as if the user indexed with the string '"61453"'.
    // Thus, the text of all numeric literals equivalent to '61543' such as '0xF00D', '0xf00D', '0170015', etc. are not valid numeric names
    // because their 'ToString' representation is not equal to their original text.
    // This is motivated by ECMA-262 sections 9.3.1, 9.8.1, 11.1.5, and 11.2.1.
    //
    // Here, we test whether 'ToString(ToNumber(name))' is exactly equal to 'name'.
    // The '+' prefix operator is equivalent here to applying the abstract ToNumber operation.
    // Applying the 'toString()' method on a number gives us the abstract ToString operation on a number.
    //
    // Note that this accepts the values 'Infinity', '-Infinity', and 'NaN', and that this is intentional.
    // This is desired behavior, because when indexing with them as numeric entities, you are indexing
    // with the strings '"Infinity"', '"-Infinity"', and '"NaN"' respectively.
    jsnum::from_string(name).string() == name
}

// utilities.go:966
pub(crate) fn is_this_property(node: P<Node>) -> bool {
    (ast::is_property_access_expression(node) || ast::is_element_access_expression(node))
        && node.expression().unwrap().kind() == Kind::ThisKeyword
}

// utilities.go:970
pub(crate) fn is_valid_number_string(s: &str, round_trip_only: bool) -> bool {
    if s.is_empty() {
        return false;
    }
    let n = jsnum::from_string(s);
    !n.is_nan() && !n.is_inf() && (!round_trip_only || n.string() == s)
}

// utilities.go:978
pub(crate) fn is_valid_big_int_string(s: &str, round_trip_only: bool) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut scanner = tsrs_scanner::Scanner::new();
    scanner.set_skip_trivia(false);
    // Go installs an error callback that clears `success`; the Rust scanner buffers errors instead.
    scanner.set_on_error(true);
    scanner.set_text(alloc_str(&format!("{s}n")));
    let mut result = scanner.scan();
    let negative = result == Kind::MinusToken;
    if negative {
        result = scanner.scan();
    }
    let success = !scanner.has_errors();
    let flags = scanner.token_flags();
    // validate that
    // * scanning proceeded without error
    // * a bigint can be scanned, and that when it is scanned, it is
    // * the full length of the input string (so the scanner is one character beyond the augmented input length)
    // * it does not contain a numeric separator (the `BigInt` constructor does not accept a numeric separator in its input)
    success
        && result == Kind::BigIntLiteral
        && scanner.token_end() == s.len() as u32 + 1
        && !flags.intersects(TokenFlags::ContainsSeparator)
        && (!round_trip_only
            || s == pseudo_big_int_to_string(jsnum::new_pseudo_big_int(
                &jsnum::parse_pseudo_big_int(scanner.token_value()),
                negative,
            )))
}

// utilities.go:1004
pub(crate) fn is_valid_es_symbol_declaration(node: P<Node>) -> bool {
    if ast::is_variable_declaration(node) {
        return ast::is_var_const(node)
            && ast::is_identifier(node.as_variable_declaration().name())
            && is_variable_declaration_in_variable_statement(node);
    }
    if ast::is_property_declaration(node) {
        return has_readonly_modifier(node) && ast::has_static_modifier(node);
    }
    ast::is_property_signature_declaration(node) && has_readonly_modifier(node)
}

// utilities.go:1014
pub(crate) fn is_variable_declaration_in_variable_statement(node: P<Node>) -> bool {
    let parent = node.parent().unwrap();
    ast::is_variable_declaration_list(parent)
        && ast::is_variable_statement(parent.parent().unwrap())
}

// utilities.go:1018
pub fn is_known_symbol(symbol: P<Symbol>) -> bool {
    is_late_bound_name(symbol.name())
}

// utilities.go:1022
pub(crate) fn is_private_identifier_symbol(symbol: Option<P<Symbol>>) -> bool {
    let Some(symbol) = symbol else {
        return false;
    };
    let name = symbol.name();
    name.len() >= 2
        && name.as_bytes()[0] == INTERNAL_SYMBOL_NAME_PREFIX_BYTE
        && name.as_bytes()[1] == b'#'
}

// utilities.go:1029
pub(crate) fn is_late_bound_name(name: &str) -> bool {
    name.len() >= 2
        && name.as_bytes()[0] == INTERNAL_SYMBOL_NAME_PREFIX_BYTE
        && name.as_bytes()[1] == b'@'
}

// utilities.go:1033
pub(crate) fn is_object_or_array_literal_type(t: P<Type>) -> bool {
    t.object_flags().intersects(ObjectFlags::ObjectLiteral | ObjectFlags::ArrayLiteral)
}

// utilities.go:1037
pub(crate) fn get_containing_class_excluding_class_decorators(node: P<Node>) -> Option<P<Node>> {
    let decorator = ast::find_ancestor_or_quit(node.parent(), |n| {
        if ast::is_class_like(n) {
            return FindAncestorResult::Quit;
        }
        if ast::is_decorator(n) {
            return FindAncestorResult::True;
        }
        FindAncestorResult::False
    });
    if let Some(decorator) = decorator {
        if ast::is_class_like(decorator.parent().unwrap()) {
            return ast::get_containing_class(decorator.parent().unwrap());
        }
        return ast::get_containing_class(decorator);
    }
    ast::get_containing_class(node)
}

// utilities.go:1056
pub(crate) fn is_this_type_parameter(t: P<Type>) -> bool {
    t.flags().intersects(TypeFlags::TypeParameter) && t.as_type_parameter().is_this_type.get()
}

// utilities.go:1060
pub(crate) fn is_class_instance_property(node: P<Node>) -> bool {
    if ast::is_in_js_file(node) && ast::is_expando_property_declaration(node) {
        let left = node.as_binary_expression().left();
        return (!ast::is_bindable_static_access_expression(
            left, false, /*excludeThisKeyword*/
        ) || !ast::is_prototype_access(left.expression().unwrap()))
            && !ast::is_bindable_static_name_expression(left, true /*excludeThisKeyword*/);
    }
    matches!(node.parent(), Some(p) if ast::is_class_like(p))
        && ast::is_property_declaration(node)
        && !ast::has_accessor_modifier(node)
}

// utilities.go:1069
pub(crate) fn is_this_initialized_object_binding_expression(node: Option<P<Node>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    if !(ast::is_shorthand_property_assignment(node) || ast::is_property_assignment(node)) {
        return false;
    }
    let grandparent = node.parent().unwrap().parent().unwrap();
    ast::is_binary_expression(grandparent)
        && grandparent.as_binary_expression().operator_token.kind() == Kind::EqualsToken
        && grandparent.as_binary_expression().right().kind() == Kind::ThisKeyword
}

// utilities.go:1075
pub(crate) fn is_this_initialized_declaration(node: Option<P<Node>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    ast::is_variable_declaration(node)
        && matches!(node.initializer(), Some(init) if init.kind() == Kind::ThisKeyword)
}

impl Checker {
    // utilities.go:1083
    pub(crate) fn is_constant_variable(&mut self, symbol: P<Symbol>) -> bool {
        symbol.flags().intersects(SymbolFlags::Variable)
            && self.get_declaration_node_flags_from_symbol(symbol).intersects(NodeFlags::Constant)
    }

    // utilities.go:1087
    pub(crate) fn is_parameter_or_mutable_local_variable(symbol: P<Symbol>) -> bool {
        // Return true if symbol is a parameter, a catch clause variable, or a mutable local variable
        if let Some(value_declaration) = symbol.value_declaration() {
            let declaration = ast::get_root_declaration(value_declaration);
            return ast::is_parameter_declaration(declaration)
                || ast::is_variable_declaration(declaration)
                    && (ast::is_catch_clause(declaration.parent().unwrap())
                        || Self::is_mutable_local_variable_declaration(declaration));
        }
        false
    }

    // utilities.go:1096
    pub(crate) fn is_mutable_local_variable_declaration(declaration: P<Node>) -> bool {
        // Return true if symbol is a non-exported and non-global `let` variable
        let parent = declaration.parent().unwrap();
        parent.flags().intersects(NodeFlags::Let)
            && !(ast::get_combined_modifier_flags(declaration).intersects(ModifierFlags::Export)
                || parent.parent().unwrap().kind() == Kind::VariableStatement
                    && ast::is_global_source_file(parent.parent().unwrap().parent().unwrap()))
    }
}

// utilities.go:1101
pub(crate) fn is_in_ambient_or_type_node(node: P<Node>) -> bool {
    node.flags().intersects(NodeFlags::Ambient)
        || ast::find_ancestor(node, |n| {
            ast::is_interface_declaration(n)
                || ast::is_type_or_js_type_alias_declaration(n)
                || ast::is_type_literal_node(n)
        })
        .is_some()
}

// utilities.go:1107
pub(crate) fn is_literal_expression_of_object(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::ObjectLiteralExpression
            | Kind::ArrayLiteralExpression
            | Kind::RegularExpressionLiteral
            | Kind::FunctionExpression
            | Kind::ClassExpression
    )
}

// utilities.go:1120
pub(crate) fn is_non_null_access(node: P<Node>) -> bool {
    ast::is_access_expression(node) && ast::is_non_null_expression(node.expression().unwrap())
}

// utilities.go:1124
pub(crate) fn get_binding_element_property_name(node: P<Node>) -> Option<P<Node>> {
    node.property_name_or_name()
}

// utilities.go:1128
pub(crate) fn is_call_chain(node: P<Node>) -> bool {
    ast::is_call_expression(node) && node.flags().intersects(NodeFlags::OptionalChain)
}

impl Checker {
    // utilities.go:1132
    pub(crate) fn call_like_expression_may_have_type_arguments(node: P<Node>) -> bool {
        ast::is_call_or_new_expression(node)
            || ast::is_tagged_template_expression(node)
            || ast::is_jsx_opening_like_element(node)
    }
}

// utilities.go:1136
pub(crate) fn is_super_call(n: P<Node>) -> bool {
    ast::is_call_expression(n) && n.expression().unwrap().kind() == Kind::SuperKeyword
}

// utilities.go:1140
pub(crate) fn get_members_of_declaration(node: P<Node>) -> Vec<P<Node>> {
    match node.kind() {
        Kind::InterfaceDeclaration
        | Kind::ClassDeclaration
        | Kind::ClassExpression
        | Kind::TypeLiteral => node.members().to_vec(),
        Kind::ObjectLiteralExpression => node.properties().to_vec(),
        _ => Vec::new(),
    }
}

// utilities.go:1150
pub(crate) fn is_in_right_side_of_import_or_export_assignment(node: P<Node>) -> bool {
    let mut node = node;
    while node.parent().unwrap().kind() == Kind::QualifiedName {
        node = node.parent().unwrap();
    }
    let parent = node.parent().unwrap();
    parent.kind() == Kind::ImportEqualsDeclaration
        && parent.as_import_equals_declaration().module_reference == node
        || parent.kind() == Kind::ExportAssignment && parent.expression() == Some(node)
}

// utilities.go:1159
pub(crate) fn is_jsx_intrinsic_tag_name(tag_name: P<Node>) -> bool {
    ast::is_identifier(tag_name) && tsrs_scanner::is_intrinsic_jsx_name(tag_name.text())
        || ast::is_jsx_namespaced_name(tag_name)
}

// utilities.go:1163
pub(crate) fn get_containing_object_literal(f: P<Node>) -> Option<P<Node>> {
    let parent = f.parent().unwrap();
    if (f.kind() == Kind::MethodDeclaration
        || f.kind() == Kind::GetAccessor
        || f.kind() == Kind::SetAccessor)
        && parent.kind() == Kind::ObjectLiteralExpression
    {
        return Some(parent);
    } else if f.kind() == Kind::FunctionExpression && parent.kind() == Kind::PropertyAssignment {
        return parent.parent();
    }
    None
}

// utilities.go:1174
pub(crate) fn is_import_type_qualifier_part(node: P<Node>) -> Option<P<Node>> {
    let mut node = node;
    let mut parent = node.parent();
    while let Some(p) = parent {
        if !ast::is_qualified_name(p) {
            break;
        }
        node = p;
        parent = p.parent();
    }

    if let Some(parent) = parent
        && parent.kind() == Kind::ImportType
        && parent.as_import_type_node().qualifier == Some(node)
    {
        return Some(parent);
    }

    None
}

// utilities.go:1188
pub(crate) fn is_in_name_of_expression_with_type_arguments_or_heritage_type_reference(
    node: P<Node>,
) -> bool {
    let mut node = node;
    while node.parent().unwrap().kind() == Kind::PropertyAccessExpression
        || node.parent().unwrap().kind() == Kind::QualifiedName
    {
        node = node.parent().unwrap();
    }

    node.parent().unwrap().kind() == Kind::ExpressionWithTypeArguments
        || ast::is_name_of_heritage_clause_type_reference(node)
}

// Indicates whether the result of an `Expression` will be unused.
// NOTE: This requires a node with a valid `parent` pointer.
// utilities.go:1203
pub(crate) fn expression_result_is_unused(node: P<Node>) -> bool {
    let mut node = node;
    loop {
        let parent = node.parent().unwrap();
        // walk up parenthesized expressions, but keep a pointer to the top-most parenthesized expression
        if ast::is_parenthesized_expression(parent) {
            node = parent;
            continue;
        }
        // result is unused in an expression statement, `void` expression, or the initializer or incrementer of a `for` loop
        if ast::is_expression_statement(parent)
            || ast::is_void_expression(parent)
            || ast::is_for_statement(parent)
                && (parent.initializer() == Some(node)
                    || parent.as_for_statement().incrementor == Some(node))
        {
            return true;
        }
        if ast::is_binary_expression(parent)
            && parent.as_binary_expression().operator_token.kind() == Kind::CommaToken
        {
            // left side of comma is always unused
            if node == parent.as_binary_expression().left() {
                return true;
            }
            // right side of comma is unused if parent is unused
            node = parent;
            continue;
        }
        return false;
    }
}

// utilities.go:1228
pub(crate) fn pseudo_big_int_to_string(value: PseudoBigInt) -> String {
    value.string()
}

// utilities.go:1232
pub(crate) fn get_super_container(node: P<Node>, stop_on_functions: bool) -> Option<P<Node>> {
    let mut node = node;
    loop {
        node = node.parent()?;
        match node.kind() {
            Kind::ComputedPropertyName => {
                node = node.parent().unwrap();
            }
            Kind::FunctionDeclaration | Kind::FunctionExpression | Kind::ArrowFunction => {
                if !stop_on_functions {
                    continue;
                }
                return Some(node);
            }
            Kind::PropertyDeclaration
            | Kind::PropertySignature
            | Kind::MethodDeclaration
            | Kind::MethodSignature
            | Kind::Constructor
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::ClassStaticBlockDeclaration => {
                return Some(node);
            }
            Kind::Decorator => {
                // Decorators are always applied outside of the body of a class or method.
                let parent = node.parent().unwrap();
                if ast::is_parameter_declaration(parent)
                    && ast::is_class_element(parent.parent().unwrap())
                {
                    // If the decorator's parent is a Parameter, we resolve the this container from
                    // the grandparent class declaration.
                    node = parent.parent().unwrap();
                } else if ast::is_class_element(parent) {
                    // If the decorator's parent is a class element, we resolve the 'this' container
                    // from the parent class declaration.
                    node = parent;
                }
            }
            _ => {}
        }
    }
}

// utilities.go:1264
pub(crate) fn for_each_yield_expression(
    body: P<Node>,
    mut visitor: impl FnMut(P<Node>) -> bool,
) -> bool {
    fn traverse(node: P<Node>, visitor: &mut dyn FnMut(P<Node>) -> bool) -> bool {
        match node.kind() {
            Kind::YieldExpression => {
                if visitor(node) {
                    return true;
                }
                let Some(operand) = node.expression() else {
                    return false;
                };
                return traverse(operand, visitor);
            }
            Kind::EnumDeclaration
            | Kind::InterfaceDeclaration
            | Kind::ModuleDeclaration
            | Kind::TypeAliasDeclaration => {
                // These are not allowed inside a generator now, but eventually they may be allowed
                // as local types. Regardless, skip them to avoid the work.
            }
            _ => {
                if ast::is_function_like(node) {
                    if let Some(name) = node.name()
                        && ast::is_computed_property_name(name)
                    {
                        // Note that we will not include methods/accessors of a class because they would require
                        // first descending into the class. This is by design.
                        return traverse(name.expression().unwrap(), visitor);
                    }
                } else if !ast::is_part_of_type_node(node) {
                    // This is the general case, which should include mostly expressions and statements.
                    // Also includes NodeArrays.
                    return node.for_each_child(&mut |child| traverse(child, visitor));
                }
            }
        }
        false
    }
    traverse(body, &mut visitor)
}

// utilities.go:1298
pub(crate) fn get_enclosing_container(node: P<Node>) -> Option<P<Node>> {
    ast::find_ancestor(node.parent(), |n| {
        tsrs_binder::get_container_flags(n).intersects(tsrs_binder::ContainerFlags::IsContainer)
    })
}

// utilities.go:1304
pub(crate) fn get_declarations_of_kind(symbol: P<Symbol>, kind: Kind) -> Vec<P<Node>> {
    symbol.declarations().iter().copied().filter(|d| d.kind() == kind).collect()
}

// utilities.go:1308
pub(crate) fn has_type(node: P<Node>) -> bool {
    node.type_node().is_some()
}

// utilities.go:1312
pub(crate) fn get_non_rest_parameter_count(sig: P<Signature>) -> usize {
    sig.parameters.get().len() - usize::from(signature_has_rest_parameter(sig))
}

// utilities.go:1316
pub(crate) fn min_and_max<T: Copy>(
    slice: &[T],
    mut get_value: impl FnMut(T) -> usize,
) -> (usize, usize) {
    let mut min_value = 0;
    let mut max_value = 0;
    for (i, &element) in slice.iter().enumerate() {
        let value = get_value(element);
        if i == 0 {
            min_value = value;
            max_value = value;
        } else {
            min_value = min_value.min(value);
            max_value = max_value.max(value);
        }
    }
    (min_value, max_value)
}

// utilities.go:1609
pub(crate) fn range_of_type_parameters(
    source_file: P<SourceFile>,
    type_parameters: P<NodeList>,
) -> TextRange {
    let text = source_file.text();
    let start = type_parameters
        .pos()
        .expect_source("parsed node lists have source positions")
        .checked_sub(1)
        .expect("type parameter list starts after its opening token");
    let end = (text.len() as u32).min(
        tsrs_scanner::skip_trivia(
            text,
            type_parameters.end().expect_source("parsed node lists have source positions"),
        ) + 1,
    );
    TextRange::new(start, end)
}

// utilities.go:1613
pub(crate) fn try_get_property_access_or_identifier_to_string(expr: P<Node>) -> String {
    if ast::is_property_access_expression(expr) {
        let base_str = try_get_property_access_or_identifier_to_string(expr.expression().unwrap());
        if !base_str.is_empty() {
            return base_str + "." + &entity_name_to_string(expr.name().unwrap());
        }
    } else if ast::is_element_access_expression(expr) {
        let base_str = try_get_property_access_or_identifier_to_string(expr.expression().unwrap());
        let argument_expression = expr.as_element_access_expression().argument_expression;
        if !base_str.is_empty() && ast::is_property_name(argument_expression) {
            return base_str
                + "."
                + &ast::get_property_name_for_property_name_node(argument_expression);
        }
    } else if ast::is_identifier(expr) {
        return expr.text().to_string();
    } else if ast::is_jsx_namespaced_name(expr) {
        return entity_name_to_string(expr);
    }
    String::new()
}

// utilities.go:1633
pub(crate) fn all_declarations_in_same_source_file(symbol: P<Symbol>) -> bool {
    let declarations = symbol.declarations();
    if declarations.len() > 1 {
        let mut source_file: Option<P<SourceFile>> = None;
        if declarations.iter().enumerate().any(|(i, &d)| {
            if i == 0 {
                source_file = ast::get_source_file_of_node(d);
            } else if ast::get_source_file_of_node(d) != source_file {
                return true;
            }

            false
        }) {
            return false;
        }
    }
    true
}

// utilities.go:1647
pub(crate) fn contains_non_missing_undefined_type(c: &Checker, t: P<Type>) -> bool {
    let candidate =
        if t.flags().intersects(TypeFlags::Union) { t.as_union_type().types.get()[0] } else { t };
    candidate.flags().intersects(TypeFlags::Undefined) && candidate != c.missing_type
}

// utilities.go:1657
pub(crate) fn get_any_import_syntax(node: P<Node>) -> Option<P<Node>> {
    match node.kind() {
        Kind::ImportEqualsDeclaration => Some(node),
        Kind::ImportClause => node.parent(),
        Kind::NamespaceImport => node.parent().unwrap().parent(),
        Kind::ImportSpecifier => node.parent().unwrap().parent().unwrap().parent(),
        _ => None,
    }
}

// A reserved member name consists of the byte 0xFE (which is an invalid UTF-8 encoding) followed by one or more
// characters where the first character is not '@' or '#'. The '@' character indicates that the name is denoted by
// a well known ES Symbol instance and the '#' character indicates that the name is a PrivateIdentifier.
// utilities.go:1677
pub(crate) fn is_reserved_member_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() >= 2 && b[0] == INTERNAL_SYMBOL_NAME_PREFIX_BYTE && b[1] != b'@' && b[1] != b'#'
}

// utilities.go:1681
pub(crate) fn introduces_arguments_exotic_object(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::MethodDeclaration
            | Kind::MethodSignature
            | Kind::Constructor
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::FunctionDeclaration
            | Kind::FunctionExpression
    )
}

// utilities.go:1690
pub(crate) fn symbols_to_array(symbols: Option<P<SymbolTable>>) -> Vec<P<Symbol>> {
    let mut result = Vec::new();
    if let Some(symbols) = symbols {
        for (id, symbol) in symbols.entries() {
            if !is_reserved_member_name(id) {
                result.push(symbol);
            }
        }
    }
    result
}

// utilities.go:1700
pub fn skip_alias(symbol: P<Symbol>, checker: &mut Checker) -> P<Symbol> {
    if symbol.flags().intersects(SymbolFlags::Alias) {
        return checker.get_aliased_symbol(symbol);
    }
    symbol
}

// True if the symbol is for an external module, as opposed to a namespace.
// utilities.go:1708
pub fn is_external_module_symbol(module_symbol: P<Symbol>) -> bool {
    module_symbol.is_external_module()
}

impl Checker {
    // utilities.go:1712
    pub(crate) fn is_canceled(&self) -> bool {
        self.ctx.as_ref().is_some_and(|ctx| ctx.err().is_some())
    }

    // utilities.go:1716
    pub(crate) fn check_not_canceled(&self) {
        assert!(!self.was_canceled, "Checker was previously cancelled");
    }
}

// utilities.go:1749
pub fn value_to_string(value: LiteralValue) -> String {
    match value {
        LiteralValue::String(value) => {
            format!("\"{}\"", escape_string(value, QuoteChar::DoubleQuote))
        }
        LiteralValue::Number(value) => value.string(),
        LiteralValue::Boolean(value) => if value { "true" } else { "false" }.to_string(),
        LiteralValue::BigInt(value) => value.string() + "n",
    }
}

// utilities.go:1763
pub(crate) fn node_starts_new_lexical_environment(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::Constructor
            | Kind::FunctionExpression
            | Kind::FunctionDeclaration
            | Kind::ArrowFunction
            | Kind::MethodDeclaration
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::ModuleDeclaration
            | Kind::SourceFile
    )
}

impl Checker {
    // Determines whether a did-you-mean error should be a suggestion in an unchecked JS file.
    // Only applies to unchecked JS files without checkJS, // @ts-check or // @ts-nocheck
    // It does not suggest when the suggestion:
    // - Is from a global file that is different from the reference file, or
    // - (optionally) Is a class, or is a this.x property access expression
    // utilities.go:1777
    pub(crate) fn is_unchecked_js_suggestion(
        &self,
        node: Option<P<Node>>,
        suggestion: Option<P<Symbol>>,
        exclude_classes: bool,
    ) -> bool {
        let file = node.and_then(ast::get_source_file_of_node);
        if let Some(file) = file
            && self.compiler_options.check_js == Tristate::Unknown
            && file.check_js_directive().is_none()
            && (file.script_kind() == ScriptKind::JS || file.script_kind() == ScriptKind::JSX)
        {
            let mut declaration_file: Option<P<SourceFile>> = None;
            if let Some(suggestion) = suggestion {
                let first_declaration = suggestion.declarations().first().copied();
                if let Some(first_declaration) = first_declaration {
                    declaration_file = ast::get_source_file_of_node(first_declaration);
                }
            }
            let suggestion_has_no_extends_or_decorators = match suggestion {
                None => true,
                Some(suggestion) => match suggestion.value_declaration() {
                    None => true,
                    Some(vd) => {
                        !ast::is_class_like(vd)
                            || !ast::get_extends_heritage_clause_elements(vd).is_empty()
                            || ast::class_or_constructor_parameter_is_decorated(false, vd)
                    }
                },
            };
            return !(Some(file) != declaration_file
                && declaration_file.is_some()
                && ast::is_global_source_file(declaration_file.unwrap().as_node())
                || exclude_classes
                    && matches!(suggestion, Some(s) if s.flags().intersects(SymbolFlags::Class))
                    && suggestion_has_no_extends_or_decorators)
                && !(matches!(node, Some(n) if exclude_classes && ast::is_property_access_expression(n) && n.expression().unwrap().kind() == Kind::ThisKeyword)
                    && suggestion_has_no_extends_or_decorators);
        }
        false
    }

    // Returns if a type is or consists of a JSLiteral object type
    // In addition to objects which are directly literals,
    // * unions where every element is a jsliteral
    // * intersections where at least one element is a jsliteral
    // * and instantiable types constrained to a jsliteral
    // Should all count as literals and not print errors on access or assignment of possibly existing properties.
    // This mirrors the behavior of the index signature propagation, to which this behaves similarly (but doesn't affect assignability or inference).
    // utilities.go:1807
    pub(crate) fn is_js_literal_type(&mut self, t: P<Type>) -> bool {
        if self.no_implicit_any {
            return false;
            // Flag is meaningless under `noImplicitAny` mode
        }
        if t.object_flags().intersects(ObjectFlags::JSLiteral) {
            return true;
        }
        if t.flags().intersects(TypeFlags::Union) {
            return t.as_union_type().types.get().iter().all(|&t| self.is_js_literal_type(t));
        }
        if t.flags().intersects(TypeFlags::Intersection) {
            return t
                .as_intersection_type()
                .types
                .get()
                .iter()
                .any(|&t| self.is_js_literal_type(t));
        }
        if t.flags().intersects(TypeFlags::Instantiable) {
            let constraint = self
                .get_resolved_base_constraint(t, crate::checker_13::BaseConstraintStack::default());
            return constraint != t && self.is_js_literal_type(constraint);
        }
        false
    }
}

// CreateModuleNotFoundChain computes the diagnostic message and arguments for a module-not-found
// error chain entry. This is shared between the checker (initial diagnostic creation) and the
// incremental builder (repopulation of cached diagnostics).
// Mirrors createModuleNotFoundChain in the TypeScript compiler's utilities.ts.
// utilities.go:1839
pub fn create_module_not_found_chain(
    program: &'static dyn Program,
    file: P<SourceFile>,
    module_reference: &str,
    mode: ModuleKind,
    package_name: &str,
) -> DiagnosticDetails {
    let mut package_name = package_name.to_string();
    let resolved_module = program.get_resolved_module(file, module_reference, mode);

    if let Some(resolved_module) = resolved_module
        && !resolved_module.alternate_result.is_empty()
    {
        if resolved_module.alternate_result.contains("/node_modules/@types/") {
            package_name =
                "@types/".to_string() + &tsrs_module::mangle_scoped_package_name(&package_name);
        }
        return DiagnosticDetails {
                message: &diagnostics::THERE_ARE_TYPES_AT_0_BUT_THIS_RESULT_COULD_NOT_BE_RESOLVED_WHEN_RESPECTING_PACKAGE_JSON_EXPORTS_THE_1_LIBRARY_MAY_NEED_TO_UPDATE_ITS_PACKAGE_JSON_OR_TYPINGS,
                args: vec![resolved_module.alternate_result.to_string(), package_name],
            };
    }

    let packages_map = program.get_packages_map();
    if packages_map.contains_key(&tsrs_module::get_types_package_name(&package_name)) {
        return DiagnosticDetails {
            message: &diagnostics::IF_THE_0_PACKAGE_ACTUALLY_EXPOSES_THIS_MODULE_CONSIDER_SENDING_A_PULL_REQUEST_TO_AMEND_HTTPS_COLON_SLASH_SLASHGITHUB_COM_SLASH_DEFINITELY_TYPED_SLASH_DEFINITELY_TYPED_SLASHTREE_SLASHMASTER_SLASHTYPES_SLASH_1,
            args: vec![package_name.clone(), tsrs_module::mangle_scoped_package_name(&package_name)],
        };
    }
    if packages_map.get(&package_name).copied().unwrap_or(false) {
        return DiagnosticDetails {
            message: &diagnostics::IF_THE_0_PACKAGE_ACTUALLY_EXPOSES_THIS_MODULE_TRY_ADDING_A_NEW_DECLARATION_D_TS_FILE_CONTAINING_DECLARE_MODULE_1,
            args: vec![package_name, module_reference.to_string()],
        };
    }
    DiagnosticDetails {
        message: &diagnostics::TRY_NPM_I_SAVE_DEV_TYPES_SLASH_1_IF_IT_EXISTS_OR_ADD_A_NEW_DECLARATION_D_TS_FILE_CONTAINING_DECLARE_MODULE_0,
        args: vec![module_reference.to_string(), tsrs_module::mangle_scoped_package_name(&package_name)],
    }
}

// CreateModeMismatchDetails computes the diagnostic message and arguments for a mode-mismatch
// error chain entry. This is shared between the checker (initial diagnostic creation) and the
// incremental builder (repopulation of cached diagnostics).
// Mirrors createModeMismatchDetails in the TypeScript compiler's utilities.ts.
// utilities.go:1875
pub fn create_mode_mismatch_details(
    program: &'static dyn Program,
    file: P<SourceFile>,
) -> DiagnosticDetails {
    let ext = tspath::try_get_extension_from_path(file.file_name());
    let target_ext = if ext == tspath::EXTENSION_TS {
        tspath::EXTENSION_MTS
    } else if ext == tspath::EXTENSION_JS {
        tspath::EXTENSION_MJS
    } else {
        ""
    };
    let meta = program.get_source_file_meta_data(file.path());
    let package_json_type = meta.package_json_type;
    let package_json_directory = meta.package_json_directory;

    if !package_json_directory.is_empty() && package_json_type.is_empty() {
        if !target_ext.is_empty() {
            return DiagnosticDetails {
                message: &diagnostics::TO_CONVERT_THIS_FILE_TO_AN_ECMA_SCRIPT_MODULE_CHANGE_ITS_FILE_EXTENSION_TO_0_OR_ADD_THE_FIELD_TYPE_COLON_MODULE_TO_1,
                args: vec![target_ext.to_string(), tspath::combine_paths(&package_json_directory, &["package.json"])],
            };
        }
        return DiagnosticDetails {
            message: &diagnostics::TO_CONVERT_THIS_FILE_TO_AN_ECMA_SCRIPT_MODULE_ADD_THE_FIELD_TYPE_COLON_MODULE_TO_0,
            args: vec![tspath::combine_paths(&package_json_directory, &["package.json"])],
        };
    }
    if !target_ext.is_empty() {
        return DiagnosticDetails {
            message: &diagnostics::TO_CONVERT_THIS_FILE_TO_AN_ECMA_SCRIPT_MODULE_CHANGE_ITS_FILE_EXTENSION_TO_0_OR_CREATE_A_LOCAL_PACKAGE_JSON_FILE_WITH_TYPE_COLON_MODULE,
            args: vec![target_ext.to_string()],
        };
    }
    DiagnosticDetails {
        message: &diagnostics::TO_CONVERT_THIS_FILE_TO_AN_ECMA_SCRIPT_MODULE_CREATE_A_LOCAL_PACKAGE_JSON_FILE_WITH_TYPE_COLON_MODULE,
        args: Vec::new(),
    }
}

// utilities.go:1906
pub(crate) fn walk_up_outer_expressions(node: P<Node>) -> Option<P<Node>> {
    let mut parent = node.parent();
    while let Some(p) = parent {
        if !ast::is_outer_expression(p, OuterExpressionKinds::All) {
            break;
        }
        parent = p.parent();
    }
    parent
}

// utilities.go:1914
pub fn get_set_accessor_value_parameter(accessor: P<Node>) -> Option<P<Node>> {
    let parameters = accessor.parameters();
    if !parameters.is_empty() {
        let has_this = parameters.len() == 2 && ast::is_this_parameter(parameters[0]);
        return Some(parameters[usize::from(has_this)]);
    }
    None
}

// utilities.go:1923
pub(crate) fn quoted_and_comma_separated(items: &[&str]) -> String {
    items.iter().map(|item| format!("'{item}'")).collect::<Vec<_>>().join(", ")
}
