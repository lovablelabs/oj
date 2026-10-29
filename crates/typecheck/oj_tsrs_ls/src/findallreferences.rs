use std::cell::{Cell, OnceCell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{
    self as ast, CheckFlags, FileReference, Kind, ModifierFlags, Node, SemanticMeaning, SourceFile,
    SubtreeFacts, Symbol, SymbolFlags,
};
use tsrs_checker::{Checker, ContextFlags, EmitResolver};
use tsrs_compiler::Program;
use tsrs_core::collections::{Set, new_set_with_size_hint};
use tsrs_core::context::Context;
use tsrs_core::{P, TextPos, TextRange, tspath};
use tsrs_lsproto as lsproto;
use tsrs_printer as printer;
use tsrs_scanner as scanner;

use crate::astnav;
use crate::crossproject::{
    CrossProjectOrchestrator, combine_implementations, combine_location_array, combine_references,
    combine_vs_references,
};
use crate::hover::get_quick_info_and_declaration_at_location;
use crate::import_tracker::{
    ExportInfo, ExportKind, ImpExpKind, ImportTracker, ImportsResult, ModuleReference,
    ModuleReferenceKind, create_import_tracker, find_module_references, get_export_info,
    get_import_or_export_symbol,
};
use crate::languageservice::LanguageService;
use crate::lsconv;
use crate::rename::node_is_eligible_for_rename;
use crate::spanmap::Feature;
use crate::utilities::{
    get_adjusted_location, get_all_super_type_nodes, get_container_node,
    get_containing_node_if_in_heritage_clause, get_containing_object_literal_element,
    get_contextual_type_from_parent_or_ancestor_type_node,
    get_intersecting_meaning_from_declarations, get_local_symbol_for_export_specifier,
    get_meaning_from_location, get_non_module_symbol_of_merged_module_symbol,
    get_parent_symbols_of_property_access, get_property_symbol_from_binding_element,
    get_property_symbol_of_object_binding_pattern_without_property_name,
    get_property_symbols_from_base_types, get_reference_at_position, get_target_label,
    is_expression_of_external_module_import_equals_declaration, is_implementation,
    is_implementation_expression, is_jump_statement_target, is_label_of_labeled_statement,
    is_literal_name_of_property_declaration_or_index_access, is_module_specifier_like,
    is_name_of_module_declaration, is_object_binding_element_without_property_name,
    is_readonly_type_operator, is_source_file_with_global_exports, is_static_symbol, is_this,
    is_type_keyword, to_context_range,
};

// === types for settings ===
// findallreferences.go:29 (Go never uses referenceUseOther)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ReferenceUse {
    #[default]
    None = 0,
    References = 2,
    Rename = 3,
}

// findallreferences.go:38 (Go's findInStrings and findInComments fields are never read)
#[derive(Clone, Copy, Default)]
pub struct RefOptions {
    pub(crate) use_: ReferenceUse, // other, references, rename
    pub(crate) implementations: bool,
    pub(crate) use_aliases_for_rename: bool, // renamed from providePrefixAndSuffixTextForRename. default: true
}

// === types for results ===

// findallreferences.go:48 (Go's unverified field is never read)
pub struct RefInfo {
    pub(crate) file: Option<P<SourceFile>>,
    pub(crate) file_name: String,
    pub(crate) reference: Option<P<FileReference>>,
}

// findallreferences.go:55
#[derive(Clone)]
pub struct SymbolAndEntries {
    definition: Option<Definition>,
    pub references: Vec<Rc<ReferenceEntry>>,
}

// findallreferences.go:60
pub fn new_symbol_and_entries(
    kind: DefinitionKind,
    node: Option<P<Node>>,
    symbol: Option<P<Symbol>>,
    references: Vec<Rc<ReferenceEntry>>,
) -> SymbolAndEntries {
    SymbolAndEntries {
        definition: Some(Definition { kind, node, symbol, triple_slash_file_ref: None }),
        references,
    }
}

// findallreferences.go:71
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DefinitionKind {
    Symbol = 0,
    Label = 1,
    Keyword = 2,
    This = 3,
    String = 4,
    TripleSlashReference = 5,
}

// findallreferences.go:82
#[derive(Clone, Copy)]
pub struct Definition {
    kind: DefinitionKind,
    symbol: Option<P<Symbol>>,
    node: Option<P<Node>>,
    triple_slash_file_ref: Option<TripleSlashDefinition>,
}

// findallreferences.go:88
#[derive(Clone, Copy)]
pub struct TripleSlashDefinition {
    reference: Option<P<FileReference>>,
    file: Option<P<SourceFile>>,
}

// findallreferences.go:93
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    None = 0,
    Range = 1,
    Node = 2,
    StringLiteral = 3,
    SearchedLocalFoundProperty = 4,
    SearchedPropertyFoundLocal = 5,
}

// Go shares `*ReferenceEntry` between result groups and fills the source file, text range and LSP location in
// lazily (resolveEntry); the lazily filled fields are cells and entries are shared through `Rc`.
// findallreferences.go:104
pub struct ReferenceEntry {
    pub(crate) kind: EntryKind,
    pub(crate) node: Option<P<Node>>,
    context: Option<P<Node>>, // !!! ContextWithStartAndEndNode, optional
    pub(crate) source_file: Cell<Option<P<SourceFile>>>,
    pub(crate) text_range: Cell<Option<TextRange>>,
    lsp_range: RefCell<Option<lsproto::Location>>,
    unmappable: Cell<bool>,
}

impl ReferenceEntry {
    fn new(kind: EntryKind, node: Option<P<Node>>, context: Option<P<Node>>) -> ReferenceEntry {
        ReferenceEntry {
            kind,
            node,
            context,
            source_file: Cell::new(None),
            text_range: Cell::new(None),
            lsp_range: RefCell::new(None),
            unmappable: Cell::new(false),
        }
    }

    // Node returns the AST node for this reference entry.
    // findallreferences.go:115
    pub fn node(&self) -> Option<P<Node>> {
        self.node
    }

    // IsNodeEntry returns true if this is a node-backed reference entry.
    // findallreferences.go:120
    fn is_node_entry(&self) -> bool {
        self.node.is_some()
    }

    fn resolved_location(&self) -> lsproto::Location {
        self.lsp_range.borrow().clone().unwrap()
    }
}

impl SymbolAndEntries {
    // References returns the reference entries for this symbol.
    // findallreferences.go:125
    pub fn references(&self) -> &[Rc<ReferenceEntry>] {
        &self.references
    }

    // DefinitionNode returns the defining AST node for this symbol, if any.
    // findallreferences.go:130
    pub fn definition_node(&self) -> Option<P<Node>> {
        let definition = self.definition.as_ref()?;
        if definition.node.is_some() {
            return definition.node;
        }
        if let Some(symbol) = definition.symbol
            && let Some(&first) = symbol.declarations().first()
        {
            return Some(first);
        }
        None
    }

    // findallreferences.go:143
    pub fn definition_symbol(&self) -> Option<P<Symbol>> {
        self.definition.as_ref()?.symbol
    }

    // findallreferences.go:150
    fn can_use_definition_symbol(&self) -> bool {
        let Some(definition) = &self.definition else {
            return false;
        };

        match definition.kind {
            DefinitionKind::Symbol | DefinitionKind::This => definition.symbol.is_some(),
            // !!! TODO: TripleSlashReference may need a file reference instead.
            // May need to return true to indicate this to be file search instead and might need to do for import stuff as well
            // For now
            _ => false,
        }
    }
}

impl LanguageService {
    // findallreferences.go:168
    pub(crate) fn get_range_of_entry(&self, entry: &ReferenceEntry) -> lsproto::Range {
        self.resolve_entry(entry).resolved_location().range
    }

    // findallreferences.go:172
    pub(crate) fn get_range_of_entry_for_feature(
        &self,
        entry: &ReferenceEntry,
        feature: Feature,
    ) -> Option<lsproto::Range> {
        let location = self.get_location_of_entry_for_feature(entry, feature)?;
        Some(location.range)
    }

    // findallreferences.go:177
    pub(crate) fn get_file_name_of_entry(&self, entry: &ReferenceEntry) -> lsproto::DocumentUri {
        self.resolve_entry(entry).resolved_location().uri
    }

    // findallreferences.go:181
    fn get_location_of_entry_for_feature(
        &self,
        entry: &ReferenceEntry,
        feature: Feature,
    ) -> Option<lsproto::Location> {
        Self::resolve_entry_source(entry);
        let (location, fidelity) = self.source_file_range_to_lsp_location_for_feature(
            entry.source_file.get().unwrap(),
            entry.text_range.get().unwrap(),
            feature,
        );
        if fidelity.is_single_segment() { Some(location) } else { None }
    }

    // findallreferences.go:187
    fn resolve_entry_source(entry: &ReferenceEntry) {
        if entry.source_file.get().is_none() {
            assert!(entry.node.is_some(), "reference entry must have a node or source file");
            entry.source_file.set(ast::get_source_file_of_node(entry.node.unwrap()));
        }
        if entry.text_range.get().is_none() {
            let text_range = get_range_of_node(
                entry.node.unwrap(),
                entry.source_file.get(),
                None, /*endNode*/
            );
            entry.text_range.set(Some(text_range));
        }
    }

    // findallreferences.go:198
    pub(crate) fn resolve_entry<'e>(&self, entry: &'e ReferenceEntry) -> &'e ReferenceEntry {
        Self::resolve_entry_source(entry);
        if entry.lsp_range.borrow().is_none() {
            let (location, fidelity) = self.source_file_range_to_lsp_location(
                entry.source_file.get().unwrap(),
                entry.text_range.get().unwrap(),
            );
            *entry.lsp_range.borrow_mut() = Some(location);
            entry.unmappable.set(!fidelity.is_single_segment());
        }
        entry
    }
}

// findallreferences.go:208
pub fn new_node_entry_with_kind(node: P<Node>, kind: EntryKind) -> Rc<ReferenceEntry> {
    let e = new_node_entry_value(node);
    Rc::new(ReferenceEntry { kind, ..e })
}

// findallreferences.go:214
pub fn new_node_entry(node: P<Node>) -> Rc<ReferenceEntry> {
    Rc::new(new_node_entry_value(node))
}

fn new_node_entry_value(node: P<Node>) -> ReferenceEntry {
    // creates nodeEntry with `kind == entryKindNode`
    ReferenceEntry::new(
        EntryKind::Node,
        Some(node.name().unwrap_or(node)),
        get_context_node_for_node_entry(node),
    )
}

// findallreferences.go:223
pub fn get_context_node_for_node_entry(node: P<Node>) -> Option<P<Node>> {
    if ast::is_declaration(node) {
        return get_context_node(Some(node));
    }

    let parent = node.parent()?;

    if !ast::is_declaration(parent) && !ast::is_export_assignment(parent) {
        // Special property assignment in javascript
        if ast::is_in_js_file(Some(node)) {
            // !!! jsdoc: check if branch still needed
            let mut binary_expression: Option<P<Node>> = None;
            if ast::is_binary_expression(parent) {
                binary_expression = Some(parent);
            } else if ast::is_access_expression(parent)
                && ast::is_binary_expression(parent.parent().unwrap())
                && parent.parent().unwrap().as_binary_expression().left == parent
            {
                binary_expression = parent.parent();
            }
            if let Some(binary_expression) = binary_expression
                && ast::get_assignment_declaration_kind(binary_expression)
                    != ast::JSDeclarationKind::None
            {
                return get_context_node(Some(binary_expression));
            }
        }

        // Jsx Tags
        match parent.kind() {
            Kind::JsxOpeningElement | Kind::JsxClosingElement => return parent.parent(),
            Kind::JsxSelfClosingElement
            | Kind::LabeledStatement
            | Kind::BreakStatement
            | Kind::ContinueStatement => return Some(parent),
            Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral => {
                if let Some(valid_import) = ast::try_get_import_from_module_specifier(node) {
                    // Go's callback ignores its argument and tests `node` (the closure's outer variable).
                    let decl_or_statement = ast::find_ancestor(Some(valid_import), |_| {
                        ast::is_declaration(node)
                            || ast::is_statement(node)
                            || ast::is_jsdoc_tag(node)
                    });
                    if decl_or_statement.is_some_and(ast::is_declaration) {
                        return get_context_node(decl_or_statement);
                    }
                    return decl_or_statement;
                }
            }
            _ => {}
        }

        // Handle computed property name
        let property_name = ast::find_ancestor(Some(node), ast::is_computed_property_name);
        if let Some(property_name) = property_name {
            return get_context_node(property_name.parent());
        }
        return None;
    }

    if parent.name() == Some(node) // node is name of declaration, use parent
        || parent.kind() == Kind::Constructor
        || parent.kind() == Kind::ExportAssignment
        // Property name of the import export specifier or binding pattern, use parent
        || ((ast::is_import_or_export_specifier(parent) || parent.kind() == Kind::BindingElement) && parent.property_name() == Some(node))
        // Is default export
        || (node.kind() == Kind::DefaultKeyword && ast::has_syntactic_modifier(parent, ModifierFlags::ExportDefault))
    {
        return get_context_node(Some(parent));
    }

    None
}

// findallreferences.go:286
pub fn get_context_node(node: Option<P<Node>>) -> Option<P<Node>> {
    let node = node?;
    match node.kind() {
        Kind::VariableDeclaration => {
            let parent = node.parent().unwrap();
            if !ast::is_variable_declaration_list(parent)
                || parent.as_variable_declaration_list().declarations.nodes().len() != 1
            {
                Some(node)
            } else if ast::is_variable_statement(parent.parent().unwrap()) {
                parent.parent()
            } else if ast::is_for_in_or_of_statement(parent.parent().unwrap()) {
                get_context_node(parent.parent())
            } else {
                Some(parent)
            }
        }

        Kind::BindingElement => get_context_node(node.parent().unwrap().parent()),

        Kind::ImportSpecifier => node.parent().unwrap().parent().unwrap().parent(),

        Kind::ExportSpecifier | Kind::NamespaceImport => node.parent().unwrap().parent(),

        Kind::ImportClause | Kind::NamespaceExport => node.parent(),

        Kind::BinaryExpression => {
            if node.parent().unwrap().kind() == Kind::ExpressionStatement {
                node.parent()
            } else {
                Some(node)
            }
        }

        Kind::ForOfStatement | Kind::ForInStatement | Kind::SwitchStatement => {
            // !!! not implemented
            None
        }

        Kind::PropertyAssignment | Kind::ShorthandPropertyAssignment => {
            if ast::is_array_literal_or_object_literal_destructuring_pattern(node.parent()) {
                return get_context_node(ast::find_ancestor(node.parent(), |node| {
                    node.kind() == Kind::BinaryExpression || ast::is_for_in_or_of_statement(node)
                }));
            }
            Some(node)
        }
        _ => Some(node),
    }
}

// findallreferences.go:335
pub fn get_range_of_node(
    node: P<Node>,
    source_file: Option<P<SourceFile>>,
    end_node: Option<P<Node>>,
) -> TextRange {
    let source_file = match source_file {
        Some(f) => f,
        None => ast::get_source_file_of_node(node).unwrap(),
    };
    let mut start = scanner::get_token_pos_of_node(node, source_file, false /*includeJsDoc*/);
    let mut end =
        end_node.unwrap_or(node).end().expect_source("reference nodes have source positions");
    if ast::is_string_literal_like(node) && (end - start) > 2 {
        assert!(end_node.is_none(), "endNode is not nil for stringLiteralLike");
        start += 1;
        end -= 1;
    }
    if let Some(end_node) = end_node
        && end_node.kind() == Kind::CaseBlock
    {
        end = end_node.pos().expect_source("reference nodes have source positions");
    }
    TextRange::new(start, end)
}

// findallreferences.go:354
pub fn is_valid_reference_position(node: P<Node>, search_symbol_name: &str) -> bool {
    match node.kind() {
        Kind::PrivateIdentifier => {
            // !!!
            // if (isJSDocMemberName(node.Parent)) {
            // 	return true;
            // }
            node.text().len() == search_symbol_name.len()
        }
        Kind::Identifier => node.text().len() == search_symbol_name.len(),
        Kind::NoSubstitutionTemplateLiteral | Kind::StringLiteral => {
            let parent = node.parent().unwrap();
            node.text().len() == search_symbol_name.len()
                && (is_literal_name_of_property_declaration_or_index_access(node)
                    || is_name_of_module_declaration(node)
                    || is_expression_of_external_module_import_equals_declaration(node)
                    || ast::is_call_expression(parent)
                        && ast::is_bindable_object_define_property_call(parent)
                        && parent.arguments()[1] == node
                    || ast::is_import_or_export_specifier(parent))
        }
        Kind::NumericLiteral => {
            is_literal_name_of_property_declaration_or_index_access(node)
                && node.text().len() == search_symbol_name.len()
        }
        Kind::DefaultKeyword => "default".len() == search_symbol_name.len(),
        _ => false,
    }
}

// findallreferences.go:378
pub fn is_for_rename_with_prefix_and_suffix_text(options: RefOptions) -> bool {
    options.use_ == ReferenceUse::Rename && options.use_aliases_for_rename
}

// findallreferences.go:382
pub fn skip_past_export_or_import_specifier_or_union(
    symbol: P<Symbol>,
    node: Option<P<Node>>,
    checker: &mut Checker,
    use_local_symbol_for_export_specifier: bool,
) -> Option<P<Symbol>> {
    let node = node?;
    let parent = node.parent().unwrap();
    if parent.kind() == Kind::ExportSpecifier && use_local_symbol_for_export_specifier {
        return get_local_symbol_for_export_specifier(node, Some(symbol), parent, checker);
    }
    // If the symbol is declared as part of a declaration like `{ type: "a" } | { type: "b" }`, use the property on the union type to get more references.
    symbol.declarations().iter().find_map(|&decl| {
        let Some(decl_parent) = decl.parent() else {
            // Ignore UMD module and global merge and CJS module end exports symbols
            if symbol.flags().intersects(SymbolFlags::Transient | SymbolFlags::ModuleExports) {
                return None;
            }
            // Assertions for GH#21814. We should be handling SourceFile symbols in `getReferencedSymbolsForModule` instead of getting here.
            panic!("Unexpected symbol at {:?}: {}", node.kind(), symbol.name());
        };
        if decl_parent.kind() == Kind::TypeLiteral
            && decl_parent.parent().unwrap().kind() == Kind::UnionType
        {
            let t = checker.get_type_from_type_node_exported(decl_parent.parent().unwrap());
            return checker.get_property_of_type_exported(t, symbol.name());
        }
        None
    })
}

// findallreferences.go:407
pub fn get_symbol_scope(symbol: P<Symbol>) -> Option<P<Node>> {
    // If this is the symbol of a named function expression or named class expression,
    // then named references are limited to its own scope.
    let value_declaration = symbol.value_declaration();
    if let Some(value_declaration) = value_declaration
        && (value_declaration.kind() == Kind::FunctionExpression
            || value_declaration.kind() == Kind::ClassExpression)
    {
        return Some(value_declaration);
    }

    if symbol.declarations().is_empty() {
        return None;
    }

    let declarations = symbol.declarations();
    // If this is private property or method, the scope is the containing class
    if symbol.flags().intersects(SymbolFlags::Property | SymbolFlags::Method) {
        let private_declaration = declarations.iter().copied().find(|&d| {
            ast::has_modifier(d, ModifierFlags::Private)
                || ast::is_private_identifier_class_element_declaration(d)
        });
        if let Some(private_declaration) = private_declaration {
            return ast::find_ancestor_kind(private_declaration, Kind::ClassDeclaration);
        }
        // Else this is a public property and could be accessed from anywhere.
        return None;
    }

    // If symbol is of object binding pattern element without property name we would want to
    // look for property too and that could be anywhere
    if declarations.iter().any(|&d| is_object_binding_element_without_property_name(d)) {
        return None;
    }

    /*
        If the symbol has a parent, it's globally visible unless:
        - It's a private property (handled above).
        - It's a type parameter.
        - The parent is an external module: then we should only search in the module (and recurse on the export later).
        - But if the parent has `export as namespace`, the symbol is globally visible through that namespace.
    */
    let exposed_by_parent =
        symbol.parent().is_some() && !symbol.flags().intersects(SymbolFlags::TypeParameter);
    if exposed_by_parent {
        let parent = symbol.parent().unwrap();
        let is_external_without_global_exports = tsrs_checker::is_external_module_symbol(parent)
            && !is_source_file_with_global_exports(parent.value_declaration());
        if !is_external_without_global_exports {
            return None;
        }
    }

    let mut scope: Option<P<Node>> = None;
    for &declaration in declarations {
        let container = get_container_node(declaration);
        if scope.is_some() && scope != container {
            // Different declarations have different containers, bail out
            return None;
        }

        match container {
            None => {
                // This is a global variable and not an external module, any declaration defined
                // within this scope is visible outside the file
                return None;
            }
            Some(container)
                if container.kind() == Kind::SourceFile
                    && !ast::is_external_or_common_js_module(container.as_source_file_p()) =>
            {
                return None;
            }
            _ => {}
        }

        scope = container;
    }

    // If symbol.parent, this means we are in an export of an external module. (Otherwise we would have returned `undefined` above.)
    // For an export of a module, we may be in a declaration file, and it may be accessed elsewhere. E.g.:
    //     declare module "a" { export type T = number; }
    //     declare module "b" { import { T } from "a"; export const x: T; }
    // So we must search the whole source file. (Because we will mark the source file as seen, we we won't return to it when searching for imports.)
    if exposed_by_parent {
        return Some(ast::get_source_file_of_node(scope.unwrap()).unwrap().as_node());
    }
    scope // TODO: GH#18217
}

// === functions on (*ls) ===

// findallreferences.go:480
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Position {
    pub(crate) uri: lsproto::DocumentUri,
    pub(crate) pos: lsproto::Position,
}

// findallreferences.go:487
impl lsproto::HasTextDocumentURI for Position {
    fn text_document_uri(&self) -> &lsproto::DocumentUri {
        &self.uri
    }
}

// findallreferences.go:488
impl lsproto::HasTextDocumentPosition for Position {
    fn text_document_position(&self) -> lsproto::Position {
        self.pos
    }
}

// Go's GetSourcePosition / GetGeneratedPosition are `sync.OnceValue` closures over the language service that
// created the definition; the port keeps their inputs and cached results and takes that service as an argument.
// findallreferences.go:490
pub struct NonLocalDefinition {
    pub(crate) position: Position,
    file_name: String,
    start_pos: TextPos,
    source_position: OnceCell<Option<Position>>,
    generated_position: OnceCell<Option<Position>>,
}

impl NonLocalDefinition {
    // findallreferences.go:524
    pub(crate) fn get_source_position(&self, l: &LanguageService) -> Option<&Position> {
        self.source_position
            .get_or_init(|| {
                let mapped = l.try_get_source_position(&self.file_name, self.start_pos);
                if let Some(mapped) = mapped {
                    let script = l.get_script(&mapped.file_name).expect("nil script");
                    let (mapped_position, mapped_fidelity) =
                        l.converters.to_lsp_position(&script, mapped.pos.into());
                    if mapped_fidelity.is_none() {
                        return None;
                    }
                    return Some(Position {
                        uri: lsconv::file_name_to_document_uri(&mapped.file_name),
                        pos: mapped_position,
                    });
                }
                None
            })
            .as_ref()
    }

    // findallreferences.go:538
    pub(crate) fn get_generated_position(&self, l: &LanguageService) -> Option<&Position> {
        self.generated_position
            .get_or_init(|| {
                let mapped = l.try_get_generated_position(&self.file_name, self.start_pos);
                if let Some(mapped) = mapped {
                    let script = l.get_script(&mapped.file_name).expect("nil script");
                    let (mapped_position, mapped_fidelity) =
                        l.converters.to_lsp_position(&script, mapped.pos.into());
                    if mapped_fidelity.is_none() {
                        return None;
                    }
                    return Some(Position {
                        uri: lsconv::file_name_to_document_uri(&mapped.file_name),
                        pos: mapped_position,
                    });
                }
                None
            })
            .as_ref()
    }
}

// findallreferences.go:496
pub fn get_file_and_start_pos_from_declaration(declaration: P<Node>) -> (P<SourceFile>, TextPos) {
    let file = ast::get_source_file_of_node(declaration).unwrap();
    // A name without a position (the `this` parameter of a JSDoc `@this` tag) starts at -1 in Go; use the
    // declaration's position instead (definition.rs create_definition_locations).
    let name = ast::get_name_of_declaration(Some(declaration))
        .filter(|name| !name.pos().is_synthetic())
        .unwrap_or(declaration);
    let text_range = get_range_of_node(name, Some(file), None /*endNode*/);

    (file, text_range.pos())
}

impl LanguageService {
    // findallreferences.go:504
    pub(crate) fn get_non_local_definition(
        &self,
        ctx: &Context,
        entry: &SymbolAndEntries,
    ) -> Option<NonLocalDefinition> {
        if !entry.can_use_definition_symbol() {
            return None;
        }

        let program = self.get_program();
        let mut checker = program.get_type_checker(ctx);
        let checker: &mut Checker = &mut checker;
        let emit_resolver = checker.new_emit_resolver(printer::new_emit_context());
        if let Some(result) =
            entry.definition.as_ref().unwrap().symbol.unwrap().declarations().iter().find_map(
                |&d| {
                    if is_definition_visible(emit_resolver, checker, d) {
                        let (file, start_pos) = get_file_and_start_pos_from_declaration(d);
                        let file_name = file.file_name();
                        let (lsp_position, fidelity) =
                            self.converters.to_lsp_position(&file, start_pos);
                        if fidelity.is_none() {
                            return None;
                        }
                        return Some(Some(NonLocalDefinition {
                            position: Position {
                                uri: lsconv::file_name_to_document_uri(file_name),
                                pos: lsp_position,
                            },
                            file_name: file_name.to_string(),
                            start_pos,
                            source_position: OnceCell::new(),
                            generated_position: OnceCell::new(),
                        }));
                    }

                    None
                },
            )
        {
            return result;
        }
        None
    }
}

// This is special handling to determine if we should load up more projects and find location in other projects
// By default arrows (and such other ast kinds) are not visible as declaration emitter doesnt need them
// But we want to handle them specially so that they are visible if their parent is visible
// findallreferences.go:561
pub fn is_definition_visible(
    emit_resolver: P<EmitResolver>,
    c: &mut Checker,
    declaration: P<Node>,
) -> bool {
    if emit_resolver.is_declaration_visible_exported(c, declaration) {
        return true;
    }
    let Some(parent) = declaration.parent() else {
        return false;
    };

    // Variable initializers are visible if variable is visible
    if ast::has_initializer(parent) && parent.initializer() == Some(declaration) {
        return is_definition_visible(emit_resolver, c, parent);
    }

    // Handle some exceptions here like arrow function, members of class and object literal expression which are technically not visible but we want the definition to be determined by its parent
    match declaration.kind() {
        Kind::PropertyDeclaration
        | Kind::GetAccessor
        | Kind::SetAccessor
        | Kind::MethodDeclaration => {
            // Private/protected properties/methods are not visible
            if ast::has_modifier(declaration, ModifierFlags::Private)
                || ast::is_private_identifier(declaration.name().unwrap())
            {
                return false;
            }
            // Public properties/methods are visible if its parents are visible, so:
            // falls through
            is_definition_visible(emit_resolver, c, parent)
        }
        Kind::Constructor
        | Kind::PropertyAssignment
        | Kind::ShorthandPropertyAssignment
        | Kind::ObjectLiteralExpression
        | Kind::ClassExpression
        | Kind::ArrowFunction
        | Kind::FunctionExpression => is_definition_visible(emit_resolver, c, parent),
        _ => false,
    }
}

impl LanguageService {
    // findallreferences.go:600
    pub(crate) fn for_each_original_definition_location(
        &self,
        _ctx: &Context,
        entry: &SymbolAndEntries,
        cb: &mut dyn FnMut(lsproto::DocumentUri, lsproto::Position),
    ) {
        if !entry.can_use_definition_symbol() {
            return;
        }

        let program = self.get_program();
        for &d in entry.definition.as_ref().unwrap().symbol.unwrap().declarations() {
            let (file, start_pos) = get_file_and_start_pos_from_declaration(d);
            let file_name = file.file_name();
            if tspath::is_declaration_file_name(file_name) {
                // Map to ts position
                let mapped = self.try_get_source_position(file.file_name(), start_pos);
                if let Some(mapped) = mapped {
                    let script = self.get_script(&mapped.file_name).expect("nil script");
                    let (lsp_position, fidelity) =
                        self.converters.to_lsp_position(&script, mapped.pos.into());
                    if !fidelity.is_none() {
                        cb(lsconv::file_name_to_document_uri(&mapped.file_name), lsp_position);
                    }
                }
            } else if program.is_source_from_project_reference(&self.to_path(file_name)) {
                let (lsp_position, fidelity) = self.converters.to_lsp_position(&file, start_pos);
                if !fidelity.is_none() {
                    cb(lsconv::file_name_to_document_uri(file_name), lsp_position);
                }
            }
        }
    }
}

// findallreferences.go:631
#[derive(Clone, Copy, Default)]
pub struct SymbolEntryTransformOptions {
    // Force the result to be Location objects.
    pub(crate) require_locations_result: bool,
    // Omit node(s) containing the original position.
    pub(crate) drop_origin_nodes: bool,
}

// findallreferences.go:638
#[derive(Clone, Default)]
pub struct SymbolAndEntriesData {
    pub(crate) original_node: Option<P<Node>>,
    pub symbols_and_entries: Vec<SymbolAndEntries>,
    position: u32,
}

impl LanguageService {
    // Go returns `(data, found)`; callers ignore `data` when not found, so the port returns `Option`.
    // findallreferences.go:644
    pub fn provide_symbols_and_entries(
        &self,
        ctx: &Context,
        uri: &lsproto::DocumentUri,
        document_position: lsproto::Position,
        is_rename: bool,
        implementations: bool,
    ) -> Option<SymbolAndEntriesData> {
        // `findReferencedSymbols` except only computes the information needed to return reference locations
        let (program, source_file) = self.get_program_and_file(uri);
        let mut feature = Feature::References;
        if implementations {
            feature = Feature::Implementation;
        } else if is_rename {
            feature = Feature::Rename;
        }
        let positions = self.converters.from_lsp_position_for_source_file(
            source_file,
            document_position,
            feature,
        );
        if positions.is_empty() {
            return None;
        }
        let mut combined = SymbolAndEntriesData::default();
        let mut ok = false;
        for mapped in positions {
            if !mapped.fidelity.is_single_segment() {
                continue;
            }
            let Some(data) = self.provide_symbols_and_entries_at_position(
                ctx,
                program,
                mapped.script,
                mapped.position,
                is_rename,
                implementations,
            ) else {
                continue;
            };
            if !ok {
                combined.original_node = data.original_node;
                combined.position = data.position;
                ok = true;
            }
            combined.symbols_and_entries.extend(data.symbols_and_entries);
        }
        if ok { Some(combined) } else { None }
    }

    // findallreferences.go:677
    pub(crate) fn provide_symbols_and_entries_at_position(
        &self,
        ctx: &Context,
        program: &'static Program,
        source_file: P<SourceFile>,
        position: u32,
        is_rename: bool,
        implementations: bool,
    ) -> Option<SymbolAndEntriesData> {
        let mut node = astnav::get_touching_property_name(source_file, position);
        if is_rename {
            // Adjust modifier/keyword nodes to the declaration name, matching Strada's findRenameLocations.
            node = get_adjusted_location(node, true /*forRename*/, Some(source_file));
        }
        if is_rename && !node_is_eligible_for_rename(Some(node))
            || implementations && ast::is_source_file(node)
        {
            return None;
        }

        let entries =
            self.get_symbol_and_entries(ctx, position, node, program, is_rename, implementations);
        if !implementations {
            return Some(SymbolAndEntriesData {
                original_node: Some(node),
                symbols_and_entries: entries,
                position,
            });
        }

        let mut implementation_entries: Vec<SymbolAndEntries> = Vec::new();
        let mut queue: VecDeque<Rc<ReferenceEntry>> = VecDeque::new();
        let mut seen_nodes: FxHashSet<Option<P<Node>>> = FxHashSet::default();
        let mut seen_definitions: FxHashSet<Option<P<Symbol>>> = FxHashSet::default();
        let mut add_to_queue =
            |symbol_and_entries: Vec<SymbolAndEntries>,
             queue: &mut VecDeque<Rc<ReferenceEntry>>| {
                for s in symbol_and_entries {
                    let mut new_references: Vec<Rc<ReferenceEntry>> = Vec::new();
                    for r in &s.references {
                        if seen_nodes.insert(r.node) {
                            queue.push_back(Rc::clone(r));
                            new_references.push(Rc::clone(r));
                        }
                    }
                    if !new_references.is_empty()
                        || s.definition.is_none()
                        || seen_definitions.insert(s.definition.as_ref().unwrap().symbol)
                    {
                        implementation_entries.push(SymbolAndEntries {
                            definition: s.definition,
                            references: new_references,
                        });
                    }
                }
            };

        add_to_queue(entries, &mut queue);
        while let Some(entry) = queue.pop_front() {
            if ctx.err().is_some() {
                return None;
            }

            if let Some(entry_node) = entry.node {
                let entries = self.get_symbol_and_entries(
                    ctx,
                    entry_node.pos().expect_source("reference entries have source positions"),
                    entry_node,
                    program,
                    is_rename,
                    implementations,
                );
                add_to_queue(entries, &mut queue);
            }
        }
        Some(SymbolAndEntriesData {
            original_node: Some(node),
            symbols_and_entries: implementation_entries,
            position,
        })
    }

    // findallreferences.go:726
    fn get_symbol_and_entries(
        &self,
        ctx: &Context,
        position: u32,
        node: P<Node>,
        program: &'static Program,
        is_rename: bool,
        implementations: bool,
    ) -> Vec<SymbolAndEntries> {
        let mut options = RefOptions::default();
        if is_rename {
            options.use_ = ReferenceUse::Rename;
            options.use_aliases_for_rename =
                self.user_preferences().use_aliases_for_rename.is_true_or_unknown();
        } else {
            options.use_ = ReferenceUse::References;
            if implementations {
                options.implementations = true;
            }
        }
        self.get_referenced_symbols_for_node(
            ctx,
            position,
            node,
            program,
            program.get_source_files(),
            options,
        )
    }

    // findallreferences.go:747
    pub fn provide_references(
        &self,
        ctx: &Context,
        params: &lsproto::ReferenceParams,
        orchestrator: Option<&dyn CrossProjectOrchestrator>,
    ) -> Result<lsproto::ReferencesResponse, lsproto::Error> {
        self.handle_cross_project(
            ctx,
            params,
            orchestrator,
            LanguageService::symbol_and_entries_to_references,
            |results| combine_references(&results),
            false, /*isRename*/
            false, /*implementations*/
            SymbolEntryTransformOptions::default(),
            None, /*defaultProjectData*/
        )
    }

    // findallreferences.go:761
    pub(crate) fn provide_references_from_data(
        &self,
        ctx: &Context,
        params: &lsproto::ReferenceParams,
        orchestrator: Option<&dyn CrossProjectOrchestrator>,
        data: &SymbolAndEntriesData,
    ) -> Result<lsproto::ReferencesResponse, lsproto::Error> {
        self.handle_cross_project(
            ctx,
            params,
            orchestrator,
            LanguageService::symbol_and_entries_to_references,
            |results| combine_references(&results),
            false, /*isRename*/
            false, /*implementations*/
            SymbolEntryTransformOptions::default(),
            Some(data),
        )
    }

    // findallreferences.go:775
    pub fn provide_vs_references(
        &self,
        ctx: &Context,
        params: &lsproto::ReferenceParams,
        orchestrator: Option<&dyn CrossProjectOrchestrator>,
    ) -> Result<lsproto::VSReferencesResponse, lsproto::Error> {
        self.handle_cross_project(
            ctx,
            params,
            orchestrator,
            LanguageService::symbol_and_entries_to_vs_references,
            combine_vs_references,
            false, /*isRename*/
            false, /*implementations*/
            SymbolEntryTransformOptions::default(),
            None, /*defaultProjectData*/
        )
    }

    // findallreferences.go:789
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    fn symbol_and_entries_to_references(
        &self,
        _ctx: &Context,
        params: &lsproto::ReferenceParams,
        data: &SymbolAndEntriesData,
        _options: SymbolEntryTransformOptions,
    ) -> Result<lsproto::ReferencesResponse, lsproto::Error> {
        // `findReferencedSymbols` except only computes the information needed to return reference locations
        let mut locations: Vec<lsproto::Location> = Vec::new();
        let mut seen_locations: Set<lsproto::Location> = Set::new();
        for symbol in &data.symbols_and_entries {
            let symbol_locations = self.convert_symbol_and_entries_to_locations(
                symbol,
                params.context.include_declaration,
                Feature::References,
            );
            locations = combine_location_array(locations, &symbol_locations, &mut seen_locations);
        }
        Ok(lsproto::LocationsOrNull { locations: Some(locations) })
    }

    // findallreferences.go:800
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    fn symbol_and_entries_to_vs_references(
        &self,
        ctx: &Context,
        _params: &lsproto::ReferenceParams,
        data: &SymbolAndEntriesData,
        _options: SymbolEntryTransformOptions,
    ) -> Result<lsproto::VSReferencesResponse, lsproto::Error> {
        let caps = lsproto::get_client_capabilities(ctx);
        let vs_capability = caps.vs_supports_visual_studio_extensions;
        let mut items: Vec<lsproto::VSReferenceItem> = Vec::new();
        let mut id: usize = 0;
        let project_name = self.project_id.0.clone();

        for s in &data.symbols_and_entries {
            let Some(definition) = &s.definition else {
                continue;
            };

            // Convert definition to info
            let Some(def_info) = self.definition_to_referenced_symbol_definition_info(
                ctx,
                definition,
                data.original_node,
                vs_capability,
                Feature::References,
            ) else {
                continue;
            };

            // Create the definition item
            let definition_id = i32::try_from(id).expect("reference ID exceeds protocol i32");
            let empty_str = String::new();
            let def_item = lsproto::VSReferenceItem {
                vs_id: definition_id,
                vs_location: def_info.location,
                vs_definition_text: def_info.display_text,
                vs_kind: Some(vec![lsproto::VSReferenceKind::UNKNOWN]),
                vs_project_name: Some(project_name.clone()),
                vs_containing_type: Some(empty_str),
                ..Default::default()
            };
            items.push(def_item);
            id += 1;

            // Create reference items grouped under the definition
            for r in &s.references {
                // Skip the declaration itself (already represented by the definition item)
                if definition.symbol.is_some()
                    && is_declaration_of_symbol(r.node, definition.symbol)
                {
                    continue;
                }

                let Some(ref_location) =
                    self.get_location_of_entry_for_feature(r, Feature::References)
                else {
                    continue;
                };

                // Determine read/write kind
                let mut kind = lsproto::VSReferenceKind::READ;
                if r.kind != EntryKind::Range
                    && r.node.is_some()
                    && ast::is_write_access_for_reference(r.node.unwrap())
                {
                    kind = lsproto::VSReferenceKind::WRITE;
                }

                let ref_item = lsproto::VSReferenceItem {
                    vs_id: i32::try_from(id).expect("reference ID exceeds protocol i32"),
                    vs_definition_id: Some(definition_id),
                    vs_location: ref_location,
                    vs_kind: Some(vec![kind]),
                    vs_project_name: Some(project_name.clone()),
                    ..Default::default()
                };
                items.push(ref_item);
                id += 1;
            }
        }

        Ok(lsproto::VSReferenceItemsOrNull { vs_reference_items: Some(items) })
    }
}

// referencedSymbolDefinitionInfo holds the computed info for a definition
// findallreferences.go:866 (Go's node field is never read)
pub struct ReferencedSymbolDefinitionInfo {
    location: lsproto::Location,
    display_text: Option<lsproto::VSClassifiedTextElement>,
}

fn single_run(
    text: &str,
    classification: lsproto::ClassificationTypeName,
) -> lsproto::VSClassifiedTextElement {
    lsproto::VSClassifiedTextElement {
        runs: vec![lsproto::VSClassifiedTextRun {
            text: text.to_string(),
            classification_type_name: classification.0.to_string(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

impl LanguageService {
    // definitionToReferencedSymbolDefinitionInfo converts a Definition to display info
    // findallreferences.go:873
    fn definition_to_referenced_symbol_definition_info(
        &self,
        ctx: &Context,
        def: &Definition,
        original_node: Option<P<Node>>,
        vs_capability: bool,
        feature: Feature,
    ) -> Option<ReferencedSymbolDefinitionInfo> {
        match def.kind {
            DefinitionKind::Symbol => {
                let symbol = def.symbol?;
                // Get display parts
                let element = self.get_definition_kind_and_display_parts(
                    ctx,
                    symbol,
                    original_node.unwrap(),
                    vs_capability,
                );

                // Get the definition node
                let node = if let Some(&decl) = symbol.declarations().first() {
                    decl.name().unwrap_or(decl)
                } else {
                    original_node.unwrap()
                };

                let loc = self.get_location_of_entry_for_feature(
                    &ReferenceEntry::new(EntryKind::Node, Some(node), None),
                    feature,
                )?;
                Some(ReferencedSymbolDefinitionInfo { location: loc, display_text: Some(element) })
            }

            DefinitionKind::Label => {
                let node = def.node?;
                let loc = self.get_location_of_entry_for_feature(
                    &ReferenceEntry::new(EntryKind::Node, Some(node), None),
                    feature,
                )?;
                Some(ReferencedSymbolDefinitionInfo {
                    location: loc,
                    display_text: Some(single_run(
                        node.text(),
                        lsproto::ClassificationTypeName::TEXT,
                    )),
                })
            }

            DefinitionKind::Keyword => {
                let node = def.node?;
                let name = scanner::token_to_string(node.kind());
                let loc = self.get_location_of_entry_for_feature(
                    &ReferenceEntry::new(EntryKind::Node, Some(node), None),
                    feature,
                )?;
                Some(ReferencedSymbolDefinitionInfo {
                    location: loc,
                    display_text: Some(single_run(name, lsproto::ClassificationTypeName::KEYWORD)),
                })
            }

            DefinitionKind::This => {
                let node = def.node?;
                let symbol = def.symbol?;
                let element =
                    self.get_definition_kind_and_display_parts(ctx, symbol, node, vs_capability);
                let loc = self.get_location_of_entry_for_feature(
                    &ReferenceEntry::new(EntryKind::Node, Some(node), None),
                    feature,
                )?;
                Some(ReferencedSymbolDefinitionInfo { location: loc, display_text: Some(element) })
            }

            DefinitionKind::String => {
                let node = def.node?;
                let loc = self.get_location_of_entry_for_feature(
                    &ReferenceEntry::new(EntryKind::Node, Some(node), None),
                    feature,
                )?;
                Some(ReferencedSymbolDefinitionInfo {
                    location: loc,
                    display_text: Some(single_run(
                        node.text(),
                        lsproto::ClassificationTypeName::STRING,
                    )),
                })
            }

            DefinitionKind::TripleSlashReference => {
                let triple_slash_file_ref = def.triple_slash_file_ref.as_ref()?;
                let file = triple_slash_file_ref.file?;
                let node = file.as_node();
                let loc = self.get_location_of_entry_for_feature(
                    &ReferenceEntry::new(EntryKind::Node, Some(node), None),
                    feature,
                )?;
                let text = format!("\"{}\"", triple_slash_file_ref.reference.unwrap().file_name);
                Some(ReferencedSymbolDefinitionInfo {
                    location: loc,
                    display_text: Some(single_run(&text, lsproto::ClassificationTypeName::STRING)),
                })
            }
        }
    }

    // getDefinitionKindAndDisplayParts returns the classified display text for a symbol definition.
    // findallreferences.go:997
    fn get_definition_kind_and_display_parts(
        &self,
        ctx: &Context,
        symbol: P<Symbol>,
        original_node: P<Node>,
        vs_capability: bool,
    ) -> lsproto::VSClassifiedTextElement {
        let program = self.get_program();
        let mut c = program.get_type_checker(ctx);
        let c: &mut Checker = &mut c;

        let meaning = get_intersecting_meaning_from_declarations(
            Some(original_node),
            symbol,
            SemanticMeaning::All,
        );

        let info = get_quick_info_and_declaration_at_location(
            c,
            Some(symbol),
            original_node,
            None,
            vs_capability,
            meaning,
        );

        if vs_capability {
            return lsproto::VSClassifiedTextElement {
                runs: info.display_parts.get_runs().to_vec(),
                ..Default::default()
            };
        }
        // Fallback: single unclassified run with the full text
        let text = info.display_parts.as_str().to_string();
        single_run(&text, lsproto::ClassificationTypeName::TEXT)
    }

    // findallreferences.go:1016
    pub fn provide_implementations(
        &self,
        ctx: &Context,
        params: &lsproto::ImplementationParams,
        orchestrator: Option<&dyn CrossProjectOrchestrator>,
    ) -> Result<lsproto::ImplementationResponse, lsproto::Error> {
        self.provide_implementations_ex(
            ctx,
            params,
            SymbolEntryTransformOptions::default(),
            orchestrator,
        )
    }

    // findallreferences.go:1020
    fn provide_implementations_ex(
        &self,
        ctx: &Context,
        params: &lsproto::ImplementationParams,
        options: SymbolEntryTransformOptions,
        orchestrator: Option<&dyn CrossProjectOrchestrator>,
    ) -> Result<lsproto::ImplementationResponse, lsproto::Error> {
        self.handle_cross_project(
            ctx,
            params,
            orchestrator,
            LanguageService::symbol_and_entries_to_implementations,
            |results| combine_implementations(&results),
            false, /*isRename*/
            true,  /*implementations*/
            options,
            None, /*defaultProjectData*/
        )
    }

    // findallreferences.go:1034
    pub(crate) fn provide_implementations_from_data(
        &self,
        ctx: &Context,
        params: &lsproto::ImplementationParams,
        options: SymbolEntryTransformOptions,
        orchestrator: Option<&dyn CrossProjectOrchestrator>,
        data: &SymbolAndEntriesData,
    ) -> Result<lsproto::ImplementationResponse, lsproto::Error> {
        self.handle_cross_project(
            ctx,
            params,
            orchestrator,
            LanguageService::symbol_and_entries_to_implementations,
            |results| combine_implementations(&results),
            false, /*isRename*/
            true,  /*implementations*/
            options,
            Some(data),
        )
    }

    // findallreferences.go:1048
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    fn symbol_and_entries_to_implementations(
        &self,
        ctx: &Context,
        _params: &lsproto::ImplementationParams,
        data: &SymbolAndEntriesData,
        options: SymbolEntryTransformOptions,
    ) -> Result<lsproto::ImplementationResponse, lsproto::Error> {
        let mut seen_nodes: FxHashSet<Option<P<Node>>> = FxHashSet::default();
        let mut entries: Vec<Rc<ReferenceEntry>> = Vec::new();
        entries.extend(
            data.symbols_and_entries
                .iter()
                .flat_map(|entry| &entry.references)
                .filter(|r| {
                    seen_nodes.insert(r.node)
                        && (!options.drop_origin_nodes
                            || !r.node.unwrap().loc().contains_inclusive(data.position.into()))
                })
                .map(Rc::clone),
        );

        if !options.require_locations_result
            && lsproto::get_client_capabilities(ctx).text_document.implementation.link_support
        {
            let links = self.convert_entries_to_location_links(&entries, Feature::Implementation);
            return Ok(lsproto::LocationOrLocationsOrDefinitionLinksOrNull {
                definition_links: Some(links),
                ..Default::default()
            });
        }
        let locations = self.convert_entries_to_locations(&entries, Feature::Implementation);
        Ok(lsproto::LocationOrLocationsOrDefinitionLinksOrNull {
            locations: Some(locations),
            ..Default::default()
        })
    }

    // == functions for conversions ==
    // findallreferences.go:1068
    fn convert_symbol_and_entries_to_locations(
        &self,
        s: &SymbolAndEntries,
        include_declarations: bool,
        feature: Feature,
    ) -> Vec<lsproto::Location> {
        let filtered: Vec<Rc<ReferenceEntry>>;
        let mut references: &[Rc<ReferenceEntry>] = &s.references;

        // !!! includeDeclarations
        if !include_declarations && let Some(definition) = &s.definition {
            filtered = s
                .references
                .iter()
                .filter(|entry| !is_declaration_of_symbol(entry.node, definition.symbol))
                .cloned()
                .collect();
            references = &filtered;
        }

        self.convert_entries_to_locations(references, feature)
    }
}

// findallreferences.go:1081
pub fn is_declaration_of_symbol(node: Option<P<Node>>, target: Option<P<Symbol>>) -> bool {
    let (Some(node), Some(target)) = (node, target) else {
        return false;
    };

    let mut source: Option<P<Node>> = None;
    if let Some(decl) = ast::get_declaration_from_name(Some(node)) {
        source = Some(decl);
    } else if node.kind() == Kind::DefaultKeyword {
        source = node.parent();
    } else if (ast::is_literal_computed_property_declaration_name(node))
        || (node.kind() == Kind::ConstructorKeyword
            && ast::is_constructor_declaration(node.parent().unwrap()))
    {
        source = node.parent().unwrap().parent();
    }

    // !!!
    // const commonjsSource = source && isBinaryExpression(source) ? source.left as unknown as Declaration : undefined;

    source.is_some() && target.declarations().iter().any(|&decl| Some(decl) == source)
}

impl LanguageService {
    // findallreferences.go:1105
    fn convert_entries_to_locations(
        &self,
        entries: &[Rc<ReferenceEntry>],
        feature: Feature,
    ) -> Vec<lsproto::Location> {
        let mut locations: Vec<lsproto::Location> = Vec::with_capacity(entries.len());
        locations.extend(
            entries
                .iter()
                .filter_map(|entry| self.get_location_of_entry_for_feature(entry, feature)),
        );
        locations
    }

    // findallreferences.go:1116
    fn convert_entries_to_location_links(
        &self,
        entries: &[Rc<ReferenceEntry>],
        feature: Feature,
    ) -> Vec<lsproto::LocationLink> {
        let mut links: Vec<lsproto::LocationLink> = Vec::with_capacity(entries.len());
        links.extend(entries.iter().filter_map(|entry| {
            // Get the selection range (the actual reference)
            let loc = self.get_location_of_entry_for_feature(entry, feature)?;
            let target_selection_range = loc.range;
            let mut target_range = target_selection_range;

            // For entries with nodes, compute ranges directly from the node
            if entry.node.is_some() {
                // Get the context range (broader scope including declaration context)
                let context_text_range = to_context_range(
                    entry.text_range.get(),
                    entry.source_file.get().unwrap(),
                    entry.context,
                );
                if let Some(context_text_range) = context_text_range {
                    let (context_location, fidelity) = self
                        .source_file_range_to_lsp_location_for_feature(
                            entry.source_file.get().unwrap(),
                            context_text_range,
                            feature,
                        );
                    if !fidelity.is_none() && context_location.uri == loc.uri {
                        target_range = context_location.range;
                    }
                }
            }

            Some(lsproto::LocationLink {
                target_uri: lsconv::file_name_to_document_uri(
                    entry.source_file.get().unwrap().original_file_name(),
                ),
                target_range,
                target_selection_range,
                ..Default::default()
            })
        }));
        links
    }

    // Go's variadic `referencesToMerge ...[]*SymbolAndEntries` is a Vec of groups.
    // findallreferences.go:1149
    fn merge_references(
        &self,
        program: &'static Program,
        references_to_merge: Vec<Vec<SymbolAndEntries>>,
    ) -> Vec<SymbolAndEntries> {
        let mut result: Vec<SymbolAndEntries> = Vec::new();
        let get_source_file_index_of_entry = |entry: &ReferenceEntry| -> Option<usize> {
            Self::resolve_entry_source(entry);
            program.source_files().iter().position(|&f| Some(f) == entry.source_file.get())
        };

        for references in references_to_merge {
            if references.is_empty() {
                continue;
            }
            if result.is_empty() {
                result = references;
                continue;
            }
            for entry in references {
                if entry.definition.is_none()
                    || entry.definition.as_ref().unwrap().kind != DefinitionKind::Symbol
                {
                    result.push(entry);
                    continue;
                }
                let symbol = entry.definition.as_ref().unwrap().symbol;
                let ref_index = result.iter().position(|r| {
                    r.definition.as_ref().is_some_and(|definition| {
                        definition.kind == DefinitionKind::Symbol && definition.symbol == symbol
                    })
                });
                let Some(ref_index) = ref_index else {
                    result.push(entry);
                    continue;
                };

                let reference = &result[ref_index];
                let mut sorted_refs: Vec<Rc<ReferenceEntry>> = reference.references.clone();
                sorted_refs.extend(entry.references.iter().cloned());
                sorted_refs.sort_by(|entry1, entry2| {
                    let entry1_file = get_source_file_index_of_entry(entry1);
                    let entry2_file = get_source_file_index_of_entry(entry2);
                    if entry1_file != entry2_file {
                        return entry1_file.cmp(&entry2_file);
                    }

                    lsproto::compare_ranges(
                        self.get_range_of_entry(entry1),
                        self.get_range_of_entry(entry2),
                    )
                });
                result[ref_index] =
                    SymbolAndEntries { definition: reference.definition, references: sorted_refs };
            }
        }
        result
    }

    // GetReferencedSymbolsForNode returns all referenced symbols and their reference entries for the given node.
    // It returns all referenced symbols and their reference entries for the given node across the provided source files.
    // (`_exported`: the unexported Go method of the same name exists.)
    // findallreferences.go:1202
    pub fn get_referenced_symbols_for_node_exported(
        &self,
        ctx: &Context,
        position: u32,
        node: P<Node>,
        source_files: &[P<SourceFile>],
    ) -> Vec<SymbolAndEntries> {
        self.get_referenced_symbols_for_node(
            ctx,
            position,
            node,
            self.get_program(),
            source_files,
            RefOptions { use_: ReferenceUse::References, ..Default::default() },
        )
    }
}

// SignatureUsage represents a single usage of a signature declaration,
// pairing the reference name node with its containing call expression (if any).
// findallreferences.go:1210
#[derive(Clone, Copy)]
pub struct SignatureUsage {
    pub name: P<Node>,         // The identifier reference node
    pub call: Option<P<Node>>, // The containing call expression, or nil if not a call usage
}

impl LanguageService {
    // GetSignatureUsages returns all usages of a signature declaration as name-call pairs.
    // For each reference to the signature's name, it returns the reference node and
    // the call expression it appears in (nil if the reference is not in a call position).
    // findallreferences.go:1218
    pub fn get_signature_usages(
        &self,
        ctx: &Context,
        signature_decl: P<Node>,
    ) -> Vec<SignatureUsage> {
        let Some(name) = signature_decl.name() else {
            return Vec::new();
        };
        if !ast::is_identifier(name) {
            return Vec::new();
        }

        let source_files = self.get_program().get_source_files();
        let entries = self.get_referenced_symbols_for_node_exported(
            ctx,
            name.pos().expect_source("signature names have source positions"),
            name,
            source_files,
        );

        // Collect all declaration name nodes for the target symbol so we can
        // filter them out — the caller wants usages, not declarations.
        let mut decl_names: FxHashSet<P<Node>> = FxHashSet::default();
        decl_names.extend(
            entries
                .iter()
                .filter_map(|entry| entry.definition.as_ref()?.symbol)
                .flat_map(|symbol| symbol.declarations().iter().filter_map(|decl| decl.name())),
        );

        let mut result: Vec<SignatureUsage> = Vec::new();
        for entry in &entries {
            result.extend(entry.references().iter().filter_map(|r| {
                if !r.is_node_entry() {
                    return None;
                }
                let node = r.node()?;
                if decl_names.contains(&node) {
                    return None;
                }

                let called = ast::climb_past_property_access(node);

                let mut call_expr: Option<P<Node>> = None;
                if let Some(called_parent) = called.parent()
                    && ast::is_call_expression(called_parent)
                    && called_parent.expression() == Some(called)
                {
                    call_expr = Some(called_parent);
                }

                Some(SignatureUsage { name: node, call: call_expr })
            }));
        }
        result
    }

    // === functions for find all ref implementation ===

    // findallreferences.go:1269
    pub(crate) fn get_referenced_symbols_for_node(
        &self,
        ctx: &Context,
        position: u32,
        node: P<Node>,
        program: &'static Program,
        source_files: &[P<SourceFile>],
        options: RefOptions,
    ) -> Vec<SymbolAndEntries> {
        let mut node = node;
        // !!! cancellationToken
        let mut source_files_set: Set<String> = new_set_with_size_hint(source_files.len());
        source_files_set.m.extend(source_files.iter().map(|file| file.file_name().to_string()));

        if options.use_ == ReferenceUse::References || options.use_ == ReferenceUse::Rename {
            node = get_adjusted_location(
                node,
                options.use_ == ReferenceUse::Rename,
                ast::get_source_file_of_node(node),
            );
        }

        let mut checker = program.get_type_checker(ctx);
        let checker: &mut Checker = &mut checker;

        if node.kind() == Kind::SourceFile {
            let resolved_ref =
                get_reference_at_position(node.as_source_file_p(), position, program);
            let Some(resolved_ref) = resolved_ref else {
                return Vec::new();
            };
            let Some(resolved_file) = resolved_ref.file else {
                return Vec::new();
            };

            if let Some(module_symbol) =
                resolved_file.as_node().symbol().map(|s| checker.get_merged_symbol_exported(s))
            {
                return Self::get_referenced_symbols_for_module(
                    checker,
                    program,
                    module_symbol,
                    false, /*excludeImportTypeOfExportEquals*/
                    source_files,
                    &source_files_set,
                );
            }

            // !!! not implemented
            // fileIncludeReasons := program.getFileIncludeReasons();
            // if (!fileIncludeReasons) {
            // 	return nil
            // }
            return vec![SymbolAndEntries {
                definition: Some(Definition {
                    kind: DefinitionKind::TripleSlashReference,
                    symbol: None,
                    node: None,
                    triple_slash_file_ref: Some(TripleSlashDefinition {
                        reference: resolved_ref.reference,
                        file: None,
                    }),
                }),
                references: get_references_for_non_module(
                    resolved_file,
                    program, /*fileIncludeReasons,*/
                ),
            }];
        }

        if !options.implementations {
            // !!! cancellationToken
            if let Some(special) = get_referenced_symbols_special(node, source_files) {
                return special;
            }
        }

        // constructors should use the class symbol, detected by name, if present
        let location =
            if node.kind() == Kind::Constructor && node.parent().unwrap().name().is_some() {
                node.parent().unwrap().name().unwrap()
            } else {
                node
            };
        let symbol = checker.get_symbol_at_location_exported(location);
        // Could not find a symbol e.g. unknown identifier
        let Some(symbol) = symbol else {
            // String literal might be a property (and thus have a symbol), so do this here rather than in getReferencedSymbolsSpecial.
            if !options.implementations && ast::is_string_literal_like(node) {
                if is_module_specifier_like(node) {
                    // !!! not implemented
                    // fileIncludeReasons := program.GetFileIncludeReasons()
                    // if referencedFile := program.GetResolvedModuleFromModuleSpecifier(node, nil /*sourceFile*/); referencedFile != nil {
                    // return []*SymbolAndEntries{{
                    // 	definition: &Definition{Kind: definitionKindString, node: node},
                    // 	references: getReferencesForNonModule(referencedFile, program /*fileIncludeReasons,*/),
                    // }}
                    // }
                    // Fall through to string literal references. This is not very likely to return
                    // anything useful, but I guess it's better than nothing, and there's an existing
                    // test that expects this to happen (fourslash/cases/untypedModuleImport.ts).
                }
                return Self::get_references_for_string_literal(ctx, node, source_files, checker);
            }
            return Vec::new();
        };

        if symbol.name() == ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS {
            let Some(parent) = symbol.parent() else {
                return Vec::new();
            };
            return Self::get_referenced_symbols_for_module(
                checker,
                program,
                parent,
                false, /*excludeImportTypeOfExportEquals*/
                source_files,
                &source_files_set,
            );
        }

        let module_references = self.get_referenced_symbols_for_module_if_declared_by_source_file(
            ctx,
            Some(symbol),
            program,
            source_files,
            checker,
            options,
            &source_files_set,
        );
        if let Some(module_references) = &module_references
            && !symbol.flags().intersects(SymbolFlags::Transient)
        {
            return module_references.clone();
        }

        let aliased_symbol =
            get_merged_aliased_symbol_of_namespace_export_declaration(node, symbol, checker);
        let module_references_of_export_target = self
            .get_referenced_symbols_for_module_if_declared_by_source_file(
                ctx,
                aliased_symbol,
                program,
                source_files,
                checker,
                options,
                &source_files_set,
            );

        let references = get_referenced_symbols_for_symbol(
            ctx,
            program,
            symbol,
            Some(node),
            source_files,
            &source_files_set,
            checker,
            options,
        );
        self.merge_references(
            program,
            vec![
                module_references.unwrap_or_default(),
                references,
                module_references_of_export_target.unwrap_or_default(),
            ],
        )
    }

    // findallreferences.go:1354
    fn get_references_for_string_literal(
        ctx: &Context,
        node: P<Node>, /*StringLiteralLike*/
        source_files: &[P<SourceFile>],
        checker: &mut Checker,
    ) -> Vec<SymbolAndEntries> {
        let t = get_contextual_type_from_parent_or_ancestor_type_node(node, checker);
        let mut references: Vec<Rc<ReferenceEntry>> = Vec::new();
        for &source_file in source_files {
            if ctx.err().is_some() {
                continue;
            }
            let possible_references = get_possible_symbol_reference_nodes(
                source_file,
                node.text(),
                None, /*container*/
            );
            references.extend(possible_references.into_iter().filter_map(|r| {
                if ast::is_string_literal_like(r) && r.text() == node.text() {
                    if let Some(t) = t {
                        let ref_type =
                            get_contextual_type_from_parent_or_ancestor_type_node(r, checker);
                        if t != checker.get_string_type()
                            && (Some(t) == ref_type
                                || is_string_literal_property_reference(r, checker))
                        {
                            Some(new_node_entry_with_kind(r, EntryKind::StringLiteral))
                        } else {
                            None
                        }
                    } else {
                        if ast::is_no_substitution_template_literal(r)
                            && !tsrs_printer::range_is_on_single_line(r.loc(), source_file)
                        {
                            return None;
                        }
                        Some(new_node_entry_with_kind(r, EntryKind::StringLiteral))
                    }
                } else {
                    None
                }
            }));
        }

        vec![SymbolAndEntries {
            definition: Some(Definition {
                kind: DefinitionKind::String,
                symbol: None,
                node: Some(node),
                triple_slash_file_ref: None,
            }),
            references,
        }]
    }
}

// findallreferences.go:1394
pub fn is_string_literal_property_reference(
    node: P<Node>, /*StringLiteralLike*/
    checker: &mut Checker,
) -> bool {
    let parent = node.parent().unwrap();
    if ast::is_property_signature_declaration(parent) {
        let t = checker.get_type_at_location(parent.parent().unwrap());
        return checker.get_property_of_type_exported(t, node.text()).is_some();
    }
    false
}

impl LanguageService {
    // Go returns nil (no module references) or a slice; `None` is Go's nil.
    // findallreferences.go:1401
    fn get_referenced_symbols_for_module_if_declared_by_source_file(
        &self,
        ctx: &Context,
        symbol: Option<P<Symbol>>,
        program: &'static Program,
        source_files: &[P<SourceFile>],
        checker: &mut Checker,
        options: RefOptions,
        source_files_set: &Set<String>,
    ) -> Option<Vec<SymbolAndEntries>> {
        let module_source_file_name: &str;
        let symbol = symbol?;
        if !symbol.flags().intersects(SymbolFlags::Module) || symbol.declarations().is_empty() {
            return None;
        }
        {
            let module_source_file =
                symbol.declarations().iter().copied().find(|&d| ast::is_source_file(d))?;
            module_source_file_name = module_source_file.as_source_file().file_name();
        }
        let export_equals = symbol
            .exports()
            .and_then(|exports| exports.lookup(ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS));
        // If exportEquals != nil, we're about to add references to `import("mod")` anyway, so don't double-count them.
        let module_references = Self::get_referenced_symbols_for_module(
            checker,
            program,
            symbol,
            export_equals.is_some(),
            source_files,
            source_files_set,
        );
        let Some(export_equals) = export_equals else {
            return Some(module_references);
        };
        if !export_equals.flags().intersects(SymbolFlags::Alias)
            || !source_files_set.has(&module_source_file_name.to_string())
        {
            return Some(module_references);
        }
        let (symbol, _) = checker.resolve_alias_exported(Some(export_equals));
        Some(self.merge_references(
            program,
            vec![
                module_references,
                get_referenced_symbols_for_symbol(
                    ctx,
                    program,
                    symbol.unwrap(),
                    None, /*node*/
                    source_files,
                    source_files_set,
                    checker, /*, cancellationToken*/
                    options,
                ),
            ],
        ))
    }
}

// Go returns nil for "not special"; `None` is Go's nil.
// findallreferences.go:1421
pub fn get_referenced_symbols_special(
    node: P<Node>,
    source_files: &[P<SourceFile>],
) -> Option<Vec<SymbolAndEntries>> {
    if is_type_keyword(node.kind()) {
        // A void expression (i.e., `void foo()`) is not special, but the `void` type is.
        if node.kind() == Kind::VoidKeyword && node.parent().unwrap().kind() == Kind::VoidExpression
        {
            return None;
        }

        // A modifier readonly (like on a property declaration) is not special;
        // a readonly type keyword (like `readonly string[]`) is.
        if node.kind() == Kind::ReadonlyKeyword && !is_readonly_type_operator(node) {
            return None;
        }
        // Likewise, when we *are* looking for a special keyword, make sure we
        // *don't* include readonly member modifiers.
        return get_all_references_for_keyword(
            source_files,
            node.kind(),
            // cancellationToken,
            node.kind() == Kind::ReadonlyKeyword,
        );
    }

    let parent = node.parent().unwrap();
    if ast::is_import_meta(parent) && parent.name() == Some(node) {
        return get_all_references_for_import_meta(source_files);
    }

    if node.kind() == Kind::StaticKeyword && parent.kind() == Kind::ClassStaticBlockDeclaration {
        return Some(vec![SymbolAndEntries {
            definition: Some(Definition {
                kind: DefinitionKind::Keyword,
                symbol: None,
                node: Some(node),
                triple_slash_file_ref: None,
            }),
            references: vec![new_node_entry(node)],
        }]);
    }

    // Labels
    if is_jump_statement_target(node) {
        // if we have a label definition, look within its statement for references, if not, then
        // the label is undefined and we have no results..
        if let Some(label_definition) = get_target_label(Some(parent), node.text()) {
            return Some(get_label_references_in_node(
                label_definition.parent().unwrap(),
                label_definition,
            ));
        }
        return None;
    }

    if is_label_of_labeled_statement(node) {
        // it is a label definition and not a target, search within the parent labeledStatement
        return Some(get_label_references_in_node(parent, node));
    }

    if is_this(node) {
        return get_references_for_this_keyword(node, source_files /*, cancellationToken*/);
    }

    if node.kind() == Kind::SuperKeyword {
        return get_references_for_super_keyword(node);
    }

    None
}

// findallreferences.go:1477
pub fn get_label_references_in_node(
    container: P<Node>,
    target_label: P<Node>,
) -> Vec<SymbolAndEntries> {
    let source_file = ast::get_source_file_of_node(container).unwrap();
    let label_name = target_label.text();
    let references: Vec<Rc<ReferenceEntry>> =
        get_possible_symbol_reference_nodes(source_file, label_name, Some(container))
            .into_iter()
            .filter_map(|node| {
                // Only pick labels that are either the target label, or have a target that is the target label
                if node == target_label
                    || (is_jump_statement_target(node)
                        && get_target_label(Some(node), label_name) == Some(target_label))
                {
                    return Some(new_node_entry(node));
                }
                None
            })
            .collect();
    vec![new_symbol_and_entries(DefinitionKind::Label, Some(target_label), None, references)]
}

// findallreferences.go:1490
pub fn get_references_for_this_keyword(
    this_or_super_keyword: P<Node>,
    source_files: &[P<SourceFile>],
) -> Option<Vec<SymbolAndEntries>> {
    let mut search_space_node = ast::get_this_container(
        this_or_super_keyword,
        false, /*includeArrowFunctions*/
        false, /*includeClassComputedPropertyName*/
    );

    // Whether 'this' occurs in a static context within a class.
    let mut static_flag = ModifierFlags::Static;
    let is_parameter_name = |node: P<Node>| -> bool {
        node.kind() == Kind::Identifier
            && node.parent().unwrap().kind() == Kind::Parameter
            && node.parent().unwrap().name() == Some(node)
    };

    match search_space_node.kind() {
        Kind::MethodDeclaration
        | Kind::MethodSignature
        | Kind::PropertyDeclaration
        | Kind::PropertySignature
        | Kind::Constructor
        | Kind::GetAccessor
        | Kind::SetAccessor => {
            if (search_space_node.kind() == Kind::MethodDeclaration
                || search_space_node.kind() == Kind::MethodSignature)
                && ast::is_object_literal_method(search_space_node)
            {
                static_flag &= search_space_node.modifier_flags();
                search_space_node = search_space_node.parent().unwrap(); // re-assign to be the owning object literals
            } else {
                static_flag &= search_space_node.modifier_flags();
                search_space_node = search_space_node.parent().unwrap(); // re-assign to be the owning class
            }
        }
        Kind::SourceFile => {
            if ast::is_external_module(search_space_node.as_source_file_p())
                || is_parameter_name(this_or_super_keyword)
            {
                return None;
            }
        }
        Kind::FunctionDeclaration | Kind::FunctionExpression => {
            // Computed properties in classes are not handled here because references to this are illegal,
            // so there is no point finding references to them.
        }
        _ => return None,
    }

    let files_to_search: Vec<P<SourceFile>> = if search_space_node.kind() == Kind::SourceFile {
        source_files.to_vec()
    } else {
        vec![ast::get_source_file_of_node(search_space_node).unwrap()]
    };
    let mut references: Vec<Rc<ReferenceEntry>> = Vec::new();
    for source_file in files_to_search {
        // cancellationToken.throwIfCancellationRequested();
        let container = if search_space_node.kind() == Kind::SourceFile {
            source_file.as_node()
        } else {
            search_space_node
        };
        references.extend(
            get_possible_symbol_reference_nodes(source_file, "this", Some(container))
                .into_iter()
                .filter_map(|node| {
                    let keep = (|| {
                        if !is_this(node) {
                            return false;
                        }
                        let container = ast::get_this_container(
                            node,  /*includeArrowFunctions*/
                            false, /*includeClassComputedPropertyName*/
                            false,
                        );
                        if !ast::can_have_symbol(container) {
                            return false;
                        }
                        match search_space_node.kind() {
                            Kind::FunctionExpression | Kind::FunctionDeclaration => {
                                search_space_node.symbol() == container.symbol()
                            }
                            Kind::MethodDeclaration | Kind::MethodSignature => {
                                ast::is_object_literal_method(search_space_node)
                                    && search_space_node.symbol() == container.symbol()
                            }
                            Kind::ClassExpression
                            | Kind::ClassDeclaration
                            | Kind::ObjectLiteralExpression => {
                                // Make sure the container belongs to the same class/object literals
                                // and has the appropriate static modifier from the original container.
                                container.parent().is_some_and(|container_parent| {
                                    ast::can_have_symbol(container_parent)
                                        && search_space_node.symbol() == container_parent.symbol()
                                        && ast::is_static(container)
                                            == (static_flag != ModifierFlags::None)
                                })
                            }
                            Kind::SourceFile => {
                                container.kind() == Kind::SourceFile
                                    && !ast::is_external_module(container.as_source_file_p())
                                    && !is_parameter_name(node)
                            }
                            _ => false,
                        }
                    })();
                    if keep { Some(new_node_entry(node)) } else { None }
                }),
        );
    }

    let this_parameter = references.iter().find_map(|r| {
        let node = r.node.unwrap();
        if node.parent().unwrap().kind() == Kind::Parameter {
            return Some(node);
        }
        None
    });
    let this_parameter = this_parameter.unwrap_or(this_or_super_keyword);
    Some(vec![new_symbol_and_entries(
        DefinitionKind::This,
        Some(this_parameter),
        search_space_node.symbol(),
        references,
    )])
}

// findallreferences.go:1568
pub fn get_references_for_super_keyword(super_keyword: P<Node>) -> Option<Vec<SymbolAndEntries>> {
    let mut search_space_node =
        ast::get_super_container(super_keyword, false /*stopOnFunctions*/)?;
    // Whether 'super' occurs in a static context within a class.
    let mut static_flag = ModifierFlags::Static;

    match search_space_node.kind() {
        Kind::PropertyDeclaration
        | Kind::PropertySignature
        | Kind::MethodDeclaration
        | Kind::MethodSignature
        | Kind::Constructor
        | Kind::GetAccessor
        | Kind::SetAccessor => {
            static_flag &= search_space_node.modifier_flags();
            search_space_node = search_space_node.parent().unwrap(); // re-assign to be the owning class
        }
        _ => return None,
    }

    let source_file = ast::get_source_file_of_node(search_space_node).unwrap();
    let references: Vec<Rc<ReferenceEntry>> =
        get_possible_symbol_reference_nodes(source_file, "super", Some(search_space_node))
            .into_iter()
            .filter_map(|node| {
                if node.kind() != Kind::SuperKeyword {
                    return None;
                }

                let container = ast::get_super_container(node, false /*stopOnFunctions*/);

                // If we have a 'super' container, we must have an enclosing class.
                // Now make sure the owning class is the same as the search-space
                // and has the same static qualifier as the original 'super's owner.
                if let Some(container) = container
                    && ast::is_static(container) == (static_flag != ModifierFlags::None)
                    && container.parent().unwrap().symbol() == search_space_node.symbol()
                {
                    return Some(new_node_entry(node));
                }
                None
            })
            .collect();

    Some(vec![new_symbol_and_entries(
        DefinitionKind::Symbol,
        None,
        search_space_node.symbol(),
        references,
    )])
}

// findallreferences.go:1604
pub fn get_all_references_for_import_meta(
    source_files: &[P<SourceFile>],
) -> Option<Vec<SymbolAndEntries>> {
    let mut references: Vec<Rc<ReferenceEntry>> = Vec::new();
    references.extend(
        source_files
            .iter()
            .flat_map(|&source_file| {
                get_possible_symbol_reference_nodes(
                    source_file,
                    "meta",
                    Some(source_file.as_node()),
                )
            })
            .filter_map(|node| {
                let parent = node.parent().unwrap();
                ast::is_import_meta(parent).then(|| new_node_entry(parent))
            }),
    );
    if references.is_empty() {
        return None;
    }
    let first = references[0].node;
    Some(vec![SymbolAndEntries {
        definition: Some(Definition {
            kind: DefinitionKind::Keyword,
            symbol: None,
            node: first,
            triple_slash_file_ref: None,
        }),
        references,
    }])
}

// findallreferences.go:1620
pub fn get_all_references_for_keyword(
    source_files: &[P<SourceFile>],
    keyword_kind: Kind,
    filter_read_only_type_operator: bool,
) -> Option<Vec<SymbolAndEntries>> {
    // references is a list of NodeEntry
    let mut references: Vec<Rc<ReferenceEntry>> = Vec::new();
    references.extend(
        source_files
            .iter()
            .flat_map(|&source_file| {
                // cancellationToken.throwIfCancellationRequested();
                get_possible_symbol_reference_nodes(
                    source_file,
                    scanner::token_to_string(keyword_kind),
                    Some(source_file.as_node()),
                )
            })
            .filter(|&reference_location| {
                reference_location.kind() == keyword_kind
                    && (!filter_read_only_type_operator
                        || is_readonly_type_operator(reference_location))
            })
            .map(new_node_entry),
    );
    if references.is_empty() {
        return None;
    }
    let first = references[0].node;
    Some(vec![new_symbol_and_entries(DefinitionKind::Keyword, first, None, references)])
}

// findallreferences.go:1637
pub fn get_possible_symbol_reference_nodes(
    source_file: P<SourceFile>,
    symbol_name: &str,
    container: Option<P<Node>>,
) -> Vec<P<Node>> {
    get_possible_symbol_reference_positions(source_file, symbol_name, container)
        .into_iter()
        .filter_map(|pos| {
            let reference_location = astnav::get_touching_property_name(source_file, pos);
            if reference_location != source_file.as_node() {
                return Some(reference_location);
            }
            None
        })
        .collect()
}

// Go `strings.Index(text[start:], needle)`.
fn index_from(text: &str, start: usize, needle: &str) -> Option<usize> {
    tsrs_core::index_bytes(&text.as_bytes()[start..], needle.as_bytes())
}

// findallreferences.go:1646
pub fn get_possible_symbol_reference_positions(
    source_file: P<SourceFile>,
    symbol_name: &str,
    container: Option<P<Node>>,
) -> Vec<u32> {
    let mut positions: Vec<u32> = Vec::new();

    // TODO: Cache symbol existence for files to save text search
    // Also, need to make this work for unicode escapes.

    // Be resilient in the face of a symbol with no name or zero length name
    if symbol_name.is_empty() {
        return positions;
    }

    let text_str = source_file.text();
    let text = text_str.as_bytes();
    let source_length = text.len();
    let symbol_name_length = symbol_name.len();

    let container = container.unwrap_or_else(|| source_file.as_node());

    // Go quirk kept: the first index is relative to container.Pos() but is used as an absolute position.
    let container_pos = usize::try_from(
        container.pos().expect_source("reference-search containers have source positions"),
    )
    .expect("source position exceeds usize");
    let Some(mut position) = index_from(text_str, container_pos, symbol_name) else {
        return positions;
    };
    let end_pos = usize::try_from(
        container.end().expect_source("reference-search containers have source positions"),
    )
    .expect("source position exceeds usize");
    while position < end_pos {
        // We found a match.  Make sure it's not part of a larger word (i.e. the char
        // before and after it have to be a non-identifier char).
        let p = position;
        let end_position = p + symbol_name_length;

        if (p == 0 || !scanner::is_identifier_part(u32::from(text[p - 1])))
            && (end_position == source_length
                || !scanner::is_identifier_part(u32::from(text[end_position])))
        {
            // Found a real match.  Keep searching.
            positions.push(u32::try_from(p).expect("source position exceeds u32"));
        }
        let start_index = p + symbol_name_length + 1;
        if start_index > text.len() {
            break;
        }
        if let Some(found_index) = index_from(text_str, start_index, symbol_name) {
            position = start_index + found_index;
        } else {
            break;
        }
    }

    positions
}

// findFirstJsxNode recursively searches for the first JSX element, self-closing element, or fragment
// findallreferences.go:1692
pub fn find_first_jsx_node(root: P<Node>) -> Option<P<Node>> {
    fn visit(node: P<Node>) -> Option<P<Node>> {
        // Check if this is a JSX node we're looking for
        match node.kind() {
            Kind::JsxElement | Kind::JsxSelfClosingElement | Kind::JsxFragment => {
                return Some(node);
            }
            _ => {}
        }

        // Skip subtree if it doesn't contain JSX
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsJsx) {
            return None;
        }

        // Traverse children to find JSX node
        let mut result: Option<P<Node>> = None;
        node.for_each_child(&mut |child| {
            result = visit(child);
            result.is_some() // Stop if found
        });
        result
    }

    visit(root)
}

// findallreferences.go:1718
pub fn get_references_for_non_module(
    _referenced_file: P<SourceFile>,
    _program: &'static Program,
) -> Vec<Rc<ReferenceEntry>> {
    // !!! not implemented
    Vec::new()
}

// findallreferences.go:1723
pub fn get_merged_aliased_symbol_of_namespace_export_declaration(
    node: P<Node>,
    symbol: P<Symbol>,
    checker: &mut Checker,
) -> Option<P<Symbol>> {
    if node.parent().is_some_and(|parent| parent.kind() == Kind::NamespaceExportDeclaration)
        && let (Some(aliased_symbol), true) = checker.resolve_alias_exported(Some(symbol))
    {
        let target_symbol = checker.get_merged_symbol_exported(aliased_symbol);
        if aliased_symbol != target_symbol {
            return Some(target_symbol);
        }
    }
    None
}

impl LanguageService {
    // findallreferences.go:1735
    fn get_referenced_symbols_for_module(
        checker: &mut Checker,
        program: &'static Program,
        symbol: P<Symbol>,
        exclude_import_type_of_export_equals: bool,
        source_files: &[P<SourceFile>],
        source_files_set: &Set<String>,
    ) -> Vec<SymbolAndEntries> {
        assert!(symbol.value_declaration().is_some());

        let module_refs = find_module_references(program, source_files, symbol, checker);
        let mut references: Vec<Rc<ReferenceEntry>> = module_refs
            .into_iter()
            .filter_map(|reference: ModuleReference| -> Option<Rc<ReferenceEntry>> {
                match reference.kind {
                    ModuleReferenceKind::Import => {
                        let literal = reference.literal.unwrap();
                        let parent = literal.parent().unwrap();
                        if ast::is_literal_type_node(parent) {
                            let import_type = parent.parent().unwrap();
                            if ast::is_import_type_node(import_type) {
                                let import_type_node = import_type.as_import_type_node();
                                if exclude_import_type_of_export_equals
                                    && import_type_node.qualifier.is_none()
                                {
                                    return None;
                                }
                            }
                        }
                        // import("foo") with no qualifier will reference the `export =` of the module, which may be referenced anyway.
                        Some(new_node_entry(literal))
                    }
                    ModuleReferenceKind::Implicit => {
                        // For implicit references (e.g., JSX runtime imports), return the first JSX node,
                        // the first statement, or the whole file
                        let mut range_node: Option<P<Node>> = None;
                        let referencing_file = reference.referencing_file.unwrap();

                        // Skip the JSX search for tslib imports
                        if reference.literal.unwrap().text() != "tslib" {
                            range_node = find_first_jsx_node(referencing_file.as_node());
                        }

                        let range_node = match range_node {
                            Some(n) => n,
                            None => {
                                if let Some(&first) = referencing_file.statements.nodes().first() {
                                    first
                                } else {
                                    referencing_file.as_node()
                                }
                            }
                        };
                        Some(new_node_entry(range_node))
                    }
                    ModuleReferenceKind::Reference => {
                        let entry = ReferenceEntry::new(EntryKind::Range, None, None);
                        entry.source_file.set(reference.referencing_file);
                        entry.text_range.set(Some(reference.ref_.unwrap().text_range));
                        Some(Rc::new(entry))
                    }
                }
            })
            .collect();

        // Add references to the module declarations themselves
        if !symbol.declarations().is_empty() {
            references.extend(symbol.declarations().iter().filter_map(|&decl| {
                match decl.kind() {
                    Kind::SourceFile => {
                        // Don't include the source file itself. (This may not be ideal behavior, but awkward to include an entire file as a reference.)
                        None
                    }
                    Kind::ModuleDeclaration => {
                        if source_files_set.has(
                            &ast::get_source_file_of_node(decl).unwrap().file_name().to_string(),
                        ) {
                            Some(new_node_entry(decl.name().unwrap()))
                        } else {
                            None
                        }
                    }
                    _ => {
                        // This may be merged with something (e.g. a class merged with a namespace).
                        None
                    }
                }
            }));
        }

        // Handle export equals declarations
        let exported = symbol
            .exports()
            .and_then(|exports| exports.lookup(ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS));
        if let Some(exported) = exported {
            references.extend(exported.declarations().iter().filter_map(|&decl| {
                let source_file = ast::get_source_file_of_node(decl).unwrap();
                if source_files_set.has(&source_file.file_name().to_string()) {
                    // At `module.exports = ...`, reference node is `module`
                    let node: P<Node> = if ast::is_binary_expression(decl)
                        && ast::is_property_access_expression(decl.as_binary_expression().left)
                    {
                        decl.as_binary_expression().left.expression().unwrap()
                    } else if ast::is_export_assignment(decl) {
                        // Find the export keyword
                        let n = astnav::find_child_of_kind(decl, Kind::ExportKeyword, source_file);
                        assert!(n.is_some(), "Expected to find export keyword");
                        n.unwrap()
                    } else {
                        ast::get_name_of_declaration(Some(decl)).unwrap_or(decl)
                    };
                    Some(new_node_entry(node))
                } else {
                    None
                }
            }));
        }

        if !references.is_empty() {
            return vec![SymbolAndEntries {
                definition: Some(Definition {
                    kind: DefinitionKind::Symbol,
                    symbol: Some(symbol),
                    node: None,
                    triple_slash_file_ref: None,
                }),
                references,
            }];
        }
        Vec::new()
    }
}

// -- Core algorithm for find all references --
// findallreferences.go:1835
pub fn get_special_search_kind(node: Option<P<Node>>) -> &'static str {
    let Some(node) = node else {
        return "none";
    };
    match node.kind() {
        Kind::Constructor | Kind::ConstructorKeyword => "constructor",
        Kind::Identifier => {
            if ast::is_class_like(node.parent().unwrap()) {
                assert_eq!(node.parent().unwrap().name(), Some(node));
                return "class";
            }
            "none"
        }
        _ => "none",
    }
}

// findallreferences.go:1853
pub fn get_referenced_symbols_for_symbol(
    ctx: &Context,
    program: &'static Program,
    original_symbol: P<Symbol>,
    node: Option<P<Node>>,
    source_files: &[P<SourceFile>],
    source_files_set: &Set<String>,
    checker: &mut Checker,
    options: RefOptions,
) -> Vec<SymbolAndEntries> {
    // Core find-all-references algorithm for a normal symbol.

    let symbol = skip_past_export_or_import_specifier_or_union(
        original_symbol,
        node,
        checker, /*useLocalSymbolForExportSpecifier*/
        !is_for_rename_with_prefix_and_suffix_text(options),
    )
    .unwrap_or(original_symbol);

    // Compute the meaning from the location and the symbol it references
    let mut search_meaning = SemanticMeaning::All;
    if options.use_ != ReferenceUse::Rename {
        search_meaning =
            get_intersecting_meaning_from_declarations(node, symbol, SemanticMeaning::All);
    }
    let mut state = new_state(
        ctx,
        program,
        source_files,
        source_files_set,
        node,
        checker,
        search_meaning,
        options,
    );

    let mut export_specifier: Option<P<Node>> = None;
    if is_for_rename_with_prefix_and_suffix_text(options) && !symbol.declarations().is_empty() {
        export_specifier =
            symbol.declarations().iter().copied().find(|&d| ast::is_export_specifier(d));
    }
    if let Some(export_specifier) = export_specifier {
        // When renaming at an export specifier, rename the export and not the thing being exported.
        let search = state.create_search(
            node,
            original_symbol,
            ImpExpKind::Unknown, /*comingFrom*/
            "",
            Vec::new(),
        );
        state.get_references_at_export_specifier(
            export_specifier.name().unwrap(),
            Some(symbol),
            export_specifier,
            &search,
            true, /*addReferencesHere*/
            true, /*alwaysGetReferences*/
        );
    } else if node.is_some_and(|node| node.kind() == Kind::DefaultKeyword)
        && symbol.name() == ast::INTERNAL_SYMBOL_NAME_DEFAULT
        && symbol.parent().is_some()
    {
        state.add_reference(node.unwrap(), symbol, EntryKind::Node);
        state.search_for_imports_of_export(
            node.unwrap(),
            symbol,
            ExportInfo {
                exporting_module_symbol: symbol.parent().unwrap(),
                export_kind: ExportKind::Default,
            },
        );
    } else {
        let all_search_symbols = state.populate_search_symbol_set(
            symbol,
            node,
            options.use_ == ReferenceUse::Rename,
            options.use_aliases_for_rename,
            options.implementations,
        );
        let search = state.create_search(
            node,
            symbol,
            ImpExpKind::Unknown, /*comingFrom*/
            "",
            all_search_symbols,
        );
        state.get_references_in_container_or_files(symbol, &search);
    }

    state.result
}

// Symbol that is currently being searched for.
// This will be replaced if we find an alias for the symbol.
// findallreferences.go:1885
pub struct RefSearch {
    // If coming from an export, we will not recursively search for the imported symbol (since that's where we came from).
    coming_from: ImpExpKind, // import, export

    symbol: P<Symbol>,
    text: String,
    escaped_text: String,

    // Only set if `options.implementations` is true. These are the symbols checked to get the implementations of a property access.
    parents: Vec<P<Symbol>>,

    all_search_symbols: Vec<P<Symbol>>,
}

impl RefSearch {
    // Whether a symbol is in the search set.
    // Do not compare directly to `symbol` because there may be related symbols to search for. See `populateSearchSymbolSet`.
    // (Go: the `includes` closure built by createSearch.)
    fn includes(&self, sym: Option<P<Symbol>>) -> bool {
        sym.is_some_and(|sym| self.all_search_symbols.contains(&sym))
    }
}

// findallreferences.go:1903
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct InheritKey {
    symbol: P<Symbol>,
    parent: P<Symbol>,
}

// findallreferences.go:1908
pub struct RefState<'a> {
    source_files: &'a [P<SourceFile>],
    source_files_set: &'a Set<String>,
    special_search_kind: &'static str, // "none", "constructor", or "class"
    checker: &'a mut Checker,
    ctx: &'a Context,
    program: &'static Program,
    search_meaning: SemanticMeaning,
    options: RefOptions,
    result: Vec<SymbolAndEntries>,
    inherits_from_cache: FxHashMap<InheritKey, bool>,
    seen_containing_type_references: FxHashSet<P<Node>>, // node seen tracker
    seen_re_export_rhs: FxHashSet<P<Node>>,              // node seen tracker
    import_tracker: Option<ImportTracker<'a>>,
    // Go maps a symbol to its `*SymbolAndEntries` in `result`; the port keeps the index into `result`.
    symbol_to_references: FxHashMap<P<Symbol>, usize>,
    source_file_to_seen_symbols: FxHashMap<P<SourceFile>, FxHashSet<P<Symbol>>>,
}

// findallreferences.go:1926
pub fn new_state<'a>(
    ctx: &'a Context,
    program: &'static Program,
    source_files: &'a [P<SourceFile>],
    source_files_set: &'a Set<String>,
    node: Option<P<Node>>,
    checker: &'a mut Checker,
    search_meaning: SemanticMeaning,
    options: RefOptions,
) -> RefState<'a> {
    RefState {
        source_files,
        source_files_set,
        special_search_kind: get_special_search_kind(node),
        checker,
        ctx,
        program,
        search_meaning,
        options,
        result: Vec::new(),
        inherits_from_cache: FxHashMap::default(),
        seen_containing_type_references: FxHashSet::default(),
        seen_re_export_rhs: FxHashSet::default(),
        import_tracker: None,
        symbol_to_references: FxHashMap::default(),
        source_file_to_seen_symbols: FxHashMap::default(),
    }
}

impl RefState<'_> {
    // findallreferences.go:1942
    fn includes_source_file(&self, source_file: P<SourceFile>) -> bool {
        self.source_files_set.has(&source_file.file_name().to_string())
    }

    // findallreferences.go:1946
    fn get_import_searches(
        &mut self,
        export_symbol: P<Symbol>,
        export_info: ExportInfo,
    ) -> ImportsResult {
        if self.import_tracker.is_none() {
            self.import_tracker = Some(create_import_tracker(
                self.ctx,
                self.program,
                self.source_files,
                self.source_files_set,
                self.checker,
            ));
        }
        let is_for_rename = self.options.use_ == ReferenceUse::Rename;
        self.import_tracker.as_ref().unwrap().call(
            self.checker,
            export_symbol,
            export_info,
            is_for_rename,
        )
    }

    // @param allSearchSymbols set of additional symbols for use by `includes`
    // findallreferences.go:1954
    fn create_search(
        &mut self,
        location: Option<P<Node>>,
        symbol: P<Symbol>,
        coming_from: ImpExpKind,
        text: &str,
        all_search_symbols: Vec<P<Symbol>>,
    ) -> RefSearch {
        // Note: if this is an external module symbol, the name doesn't include quotes.
        // Note: getLocalSymbolForExportDefault handles `export default class C {}`, but not `export default C` or `export { C as default }`.
        // The other two forms seem to be handled downstream (e.g. in `skipPastExportOrImportSpecifier`), so special-casing the first form
        // here appears to be intentional).
        let mut text = text.to_string();
        if text.is_empty() {
            let s = tsrs_binder::get_local_symbol_for_export_default(symbol)
                .or_else(|| get_non_module_symbol_of_merged_module_symbol(symbol))
                .unwrap_or(symbol);
            let symbol_name = ast::symbol_name(s);
            if let Some(module_name) =
                ast::try_get_ambient_module_name_from_symbol_name(symbol_name)
            {
                text = module_name.to_string();
            } else {
                text = symbol_name.to_string();
            }
        }
        let mut all_search_symbols = all_search_symbols;
        if all_search_symbols.is_empty() {
            all_search_symbols = vec![symbol];
        }
        let mut search = RefSearch {
            symbol,
            coming_from,
            escaped_text: text.clone(),
            text,
            all_search_symbols,
            parents: Vec::new(),
        };
        if self.options.implementations
            && let Some(location) = location
        {
            search.parents = get_parent_symbols_of_property_access(location, symbol, self.checker);
        }
        search
    }

    // Go returns a closure that appends to the group; the port returns the group's index in `result`
    // (see `add_to_group`).
    // findallreferences.go:1991
    fn reference_adder(&mut self, search_symbol: P<Symbol>) -> usize {
        *self.symbol_to_references.entry(search_symbol).or_insert_with(|| {
            let symbol_and_entries = new_symbol_and_entries(
                DefinitionKind::Symbol,
                None,
                Some(search_symbol),
                Vec::new(),
            );
            self.result.push(symbol_and_entries);
            self.result.len() - 1
        })
    }

    fn add_to_group(&mut self, index: usize, node: P<Node>, kind: EntryKind) {
        self.result[index].references.push(new_node_entry_with_kind(node, kind));
    }

    // findallreferences.go:2003
    fn add_reference(&mut self, reference_location: P<Node>, symbol: P<Symbol>, kind: EntryKind) {
        // if rename symbol from default export anonymous function, for example `export default function() {}`, we do not need to add reference
        if self.options.use_ == ReferenceUse::Rename
            && reference_location.kind() == Kind::DefaultKeyword
        {
            return;
        }

        let add_ref = self.reference_adder(symbol);
        if self.options.implementations {
            let mut nodes: Vec<P<Node>> = Vec::new();
            self.add_implementation_references(reference_location, &mut |n| nodes.push(n));
            for n in nodes {
                self.add_to_group(add_ref, n, kind);
            }
        } else {
            self.add_to_group(add_ref, reference_location, kind);
        }
    }
}

// findallreferences.go:2017
pub fn get_reference_entries_for_shorthand_property_assignment(
    node: P<Node>,
    checker: &mut Checker,
    add_reference: &mut dyn FnMut(P<Node>),
) {
    let Some(ref_symbol) = checker.get_symbol_at_location_exported(node) else {
        return;
    };
    let Some(value_declaration) = ref_symbol.value_declaration() else {
        return;
    };
    let shorthand_symbol = checker.get_shorthand_assignment_value_symbol(Some(value_declaration));
    if let Some(shorthand_symbol) = shorthand_symbol {
        for &declaration in shorthand_symbol.declarations() {
            if ast::get_meaning_from_declaration(declaration).intersects(SemanticMeaning::Value) {
                add_reference(declaration);
            }
        }
    }
}

// findallreferences.go:2032
pub fn is_method_or_accessor(node: P<Node>) -> bool {
    node.kind() == Kind::MethodDeclaration
        || node.kind() == Kind::GetAccessor
        || node.kind() == Kind::SetAccessor
}

// findallreferences.go:2036
pub fn try_get_class_by_extending_identifier(
    node: P<Node>,
) -> Option<P<Node> /*ClassLikeDeclaration*/> {
    ast::try_get_class_extending_expression_with_type_arguments(
        ast::climb_past_property_access(node).parent().unwrap(),
    )
}

// findallreferences.go:2040
pub fn get_class_constructor_symbol(class_symbol: P<Symbol>) -> Option<P<Symbol>> {
    let members = class_symbol.members()?;
    members.lookup(ast::INTERNAL_SYMBOL_NAME_CONSTRUCTOR)
}

// findallreferences.go:2047
pub fn has_own_constructor(class_declaration: P<Node> /*ClassLikeDeclaration*/) -> bool {
    get_class_constructor_symbol(class_declaration.symbol().unwrap()).is_some()
}

// findallreferences.go:2051
pub fn find_own_constructor_references(
    class_symbol: P<Symbol>,
    source_file: P<SourceFile>,
    add_node: &mut dyn FnMut(P<Node>),
) {
    let constructor_symbol = get_class_constructor_symbol(class_symbol);
    if let Some(constructor_symbol) = constructor_symbol {
        for &decl in constructor_symbol.declarations() {
            if decl.kind() == Kind::Constructor
                && let Some(ctr_keyword) =
                    astnav::find_child_of_kind(decl, Kind::ConstructorKeyword, source_file)
            {
                add_node(ctr_keyword);
            }
        }
    }

    if let Some(exports) = class_symbol.exports() {
        for member in exports.values() {
            let decl = member.value_declaration();
            if let Some(decl) = decl
                && decl.kind() == Kind::MethodDeclaration
            {
                let body = decl.body();
                if let Some(body) = body {
                    for_each_descendant_of_kind(body, Kind::ThisKeyword, &mut |this_keyword| {
                        if ast::is_new_expression_target(this_keyword, false, false) {
                            add_node(this_keyword);
                        }
                    });
                }
            }
        }
    }
}

// findallreferences.go:2080
pub fn find_super_constructor_accesses(
    class_declaration: P<Node>, /*ClassLikeDeclaration*/
    add_node: &mut dyn FnMut(P<Node>),
) {
    let Some(constructor_symbol) =
        get_class_constructor_symbol(class_declaration.symbol().unwrap())
    else {
        return;
    };
    if constructor_symbol.declarations().is_empty() {
        return;
    }

    for &decl in constructor_symbol.declarations() {
        if decl.kind() == Kind::Constructor {
            let body = decl.body();
            if let Some(body) = body {
                for_each_descendant_of_kind(body, Kind::SuperKeyword, &mut |node| {
                    if ast::is_call_expression_target(node, false, false) {
                        add_node(node);
                    }
                });
            }
        }
    }
}

// findallreferences.go:2100
pub fn for_each_descendant_of_kind(node: P<Node>, kind: Kind, action: &mut dyn FnMut(P<Node>)) {
    node.for_each_child(&mut |child| {
        if child.kind() == kind {
            action(child);
        }
        for_each_descendant_of_kind(child, kind, action);
        false
    });
}

impl RefState<'_> {
    // findallreferences.go:2110
    fn add_implementation_references(
        &mut self,
        ref_node: P<Node>,
        add_ref: &mut dyn FnMut(P<Node>),
    ) {
        // Check if we found a function/propertyAssignment/method with an implementation or initializer
        if ast::is_declaration_name(ref_node) && is_implementation(ref_node.parent().unwrap()) {
            add_ref(ref_node);
            return;
        }

        if ref_node.kind() != Kind::Identifier {
            return;
        }

        if ref_node.parent().unwrap().kind() == Kind::ShorthandPropertyAssignment {
            // Go ahead and dereference the shorthand assignment by going to its definition
            get_reference_entries_for_shorthand_property_assignment(
                ref_node,
                self.checker,
                add_ref,
            );
        }

        // Check if the node is within an extends or implements clause

        if let Some(containing_node) = get_containing_node_if_in_heritage_clause(ref_node) {
            add_ref(containing_node);
            return;
        }

        // If we got a type reference, try and see if the reference applies to any expressions that can implement an interface
        // Find the first node whose parent isn't a type node -- i.e., the highest type node.
        let type_node = ast::find_ancestor(Some(ref_node), |a| {
            let a_parent = a.parent().unwrap();
            !ast::is_qualified_name(a_parent)
                && !ast::is_type_node(a_parent)
                && !ast::is_type_element(a_parent)
        });

        let Some(type_node) = type_node else {
            return;
        };
        if type_node.parent().unwrap().type_node().is_none() {
            return;
        }

        let type_having_node = type_node.parent().unwrap();
        if type_having_node.type_node() == Some(type_node)
            && self.seen_containing_type_references.insert(type_having_node)
        {
            let mut add_if_implementation = |e: P<Node>| {
                if is_implementation_expression(e) {
                    add_ref(e);
                }
            };
            if ast::has_initializer(type_having_node) {
                add_if_implementation(type_having_node.initializer().unwrap());
            } else if ast::is_function_like(Some(type_having_node))
                && type_having_node.body().is_some()
            {
                let body = type_having_node.body().unwrap();
                if body.kind() == Kind::Block {
                    ast::for_each_return_statement(body, |return_statement: P<Node>| {
                        if let Some(expr) = return_statement.expression() {
                            add_if_implementation(expr);
                        }
                        false
                    });
                } else {
                    add_if_implementation(body);
                }
            } else if ast::is_assertion_expression(type_having_node)
                || ast::is_satisfies_expression(type_having_node)
            {
                add_if_implementation(type_having_node.expression().unwrap());
            }
        }
    }

    // findallreferences.go:2170
    fn get_references_in_container_or_files(&mut self, symbol: P<Symbol>, search: &RefSearch) {
        // Try to get the smallest valid scope that we can limit our search to;
        // otherwise we'll need to search globally (i.e. include each file).
        if let Some(scope) = get_symbol_scope(symbol) {
            let add_references_here = scope.kind() != Kind::SourceFile
                || self.source_files.contains(&scope.as_source_file_p());
            self.get_references_in_container(
                scope,
                ast::get_source_file_of_node(scope).unwrap(),
                search,
                add_references_here,
            );
        } else {
            // Global search
            for &source_file in self.source_files {
                // state.cancellationToken.throwIfCancellationRequested();
                self.search_for_name(source_file, search);
            }
        }
    }

    // findallreferences.go:2185
    fn get_references_in_source_file(
        &mut self,
        source_file: P<SourceFile>,
        search: &RefSearch,
        add_references_here: bool,
    ) {
        // state.cancellationToken.throwIfCancellationRequested();
        self.get_references_in_container(
            source_file.as_node(),
            source_file,
            search,
            add_references_here,
        );
    }

    // findallreferences.go:2190
    fn get_references_in_container(
        &mut self,
        container: P<Node>,
        source_file: P<SourceFile>,
        search: &RefSearch,
        add_references_here: bool,
    ) {
        // Search within node "container" for references for a search value, where the search value is defined as a
        //     tuple of (searchSymbol, searchText, searchLocation, and searchMeaning).
        // searchLocation: a node where the search value
        if !self.mark_searched_symbols(source_file, &search.all_search_symbols) {
            return;
        }

        for position in
            get_possible_symbol_reference_positions(source_file, &search.text, Some(container))
        {
            self.get_references_at_location(source_file, position, search, add_references_here);
        }
    }

    // findallreferences.go:2203
    fn mark_searched_symbols(&mut self, source_file: P<SourceFile>, symbols: &[P<Symbol>]) -> bool {
        let seen_symbols = self.source_file_to_seen_symbols.entry(source_file).or_default();
        let mut any_new_symbols = false;
        any_new_symbols = symbols.iter().fold(any_new_symbols, |mut any_new_symbols, &sym| {
            if seen_symbols.insert(sym) {
                any_new_symbols = true;
            }

            any_new_symbols
        });
        any_new_symbols
    }

    // findallreferences.go:2218
    fn get_references_at_location(
        &mut self,
        source_file: P<SourceFile>,
        position: u32,
        search: &RefSearch,
        add_references_here: bool,
    ) {
        let reference_location = astnav::get_touching_property_name(source_file, position);

        if !is_valid_reference_position(reference_location, &search.text) {
            // This wasn't the start of a token.  Check to see if it might be a
            // match in a comment or string if that's what the caller is asking
            // for.

            // !!! not implemented
            // if (!state.options.implementations && (state.options.findInStrings && isInString(sourceFile, position) || state.options.findInComments && isInNonReferenceComment(sourceFile, position))) {
            // 	// In the case where we're looking inside comments/strings, we don't have
            // 	// an actual definition.  So just use 'undefined' here.  Features like
            // 	// 'Rename' won't care (as they ignore the definitions), and features like
            // 	// 'FindReferences' will just filter out these results.
            // 	state.addStringOrCommentReference(sourceFile.FileName, createTextSpan(position, search.text.length));
            // }

            return;
        }

        if !get_meaning_from_location(reference_location).intersects(self.search_meaning) {
            return;
        }

        let Some(mut reference_symbol) =
            self.checker.get_symbol_at_location_exported(reference_location)
        else {
            return;
        };

        let parent = reference_location.parent().unwrap();
        if parent.kind() == Kind::ImportSpecifier
            && parent.property_name() == Some(reference_location)
        {
            // This is added through `singleReferences` in ImportsResult. If we happen to see it again, don't add it again.
            return;
        }

        if parent.kind() == Kind::ExportSpecifier {
            self.get_references_at_export_specifier(
                reference_location,
                Some(reference_symbol),
                parent,
                search,
                add_references_here,
                false, /*alwaysGetReferences*/
            );
            return;
        }

        let (related_symbol, related_symbol_kind) =
            self.get_related_symbol(search, reference_symbol, reference_location);
        let Some(related_symbol) = related_symbol else {
            self.get_reference_for_shorthand_property(reference_symbol, search);
            return;
        };

        match self.special_search_kind {
            "none" => {
                if add_references_here {
                    self.add_reference(reference_location, related_symbol, related_symbol_kind);
                }
            }
            "constructor" => self.add_constructor_references(
                reference_location,
                related_symbol,
                search,
                add_references_here,
            ),
            "class" => self.add_class_static_this_references(
                reference_location,
                related_symbol,
                search,
                add_references_here,
            ),
            _ => {}
        }

        // Use the parent symbol if the location is commonjs require syntax on javascript files only.
        if ast::is_in_js_file(Some(reference_location))
            && reference_location.parent().unwrap().kind() == Kind::BindingElement
            && ast::is_variable_declaration_initialized_to_bare_or_accessed_require(
                reference_location.parent().unwrap().parent().unwrap().parent().unwrap(),
            )
        {
            // The parent will not have a symbol if it's an ObjectBindingPattern (when destructuring is used).  In
            // this case, just skip it, since the bound identifiers are not an alias of the import.
            let Some(parent_symbol) = reference_location.parent().unwrap().symbol() else {
                return;
            };
            reference_symbol = parent_symbol;
        }

        self.get_import_or_export_references(reference_location, reference_symbol, search);
    }

    // findallreferences.go:2289
    fn add_constructor_references(
        &mut self,
        reference_location: P<Node>,
        symbol: P<Symbol>,
        search: &RefSearch,
        add_references_here: bool,
    ) {
        if ast::is_new_expression_target(reference_location, false, false) && add_references_here {
            self.add_reference(reference_location, symbol, EntryKind::Node);
        }

        if ast::is_class_like(reference_location.parent().unwrap()) {
            // This is the class declaration containing the constructor.
            let source_file = ast::get_source_file_of_node(reference_location).unwrap();
            let mut nodes: Vec<P<Node>> = Vec::new();
            find_own_constructor_references(search.symbol, source_file, &mut |n| nodes.push(n));
            for n in nodes {
                let pusher = self.reference_adder(search.symbol);
                self.add_to_group(pusher, n, EntryKind::Node);
            }
        } else {
            // If this class appears in `extends C`, then the extending class' "super" calls are references.
            if let Some(class_extending) = try_get_class_by_extending_identifier(reference_location)
            {
                let mut nodes: Vec<P<Node>> = Vec::new();
                find_super_constructor_accesses(class_extending, &mut |n| nodes.push(n));
                for n in nodes {
                    let pusher = self.reference_adder(search.symbol);
                    self.add_to_group(pusher, n, EntryKind::Node);
                }
                self.find_inherited_constructor_references(class_extending);
            }
        }
    }

    // findallreferences.go:2315
    fn add_class_static_this_references(
        &mut self,
        reference_location: P<Node>,
        symbol: P<Symbol>,
        search: &RefSearch,
        add_references_here: bool,
    ) {
        if add_references_here {
            self.add_reference(reference_location, symbol, EntryKind::Node);
        }

        let class_like = reference_location.parent().unwrap();
        if self.options.use_ == ReferenceUse::Rename || !ast::is_class_like(class_like) {
            return;
        }

        let add_ref = self.reference_adder(search.symbol);
        let members = class_like.members();
        for &member in members {
            if !(is_method_or_accessor(member) && ast::has_static_modifier(member)) {
                continue;
            }
            let body = member.body();
            if let Some(body) = body {
                fn cb(node: P<Node>, found: &mut Vec<P<Node>>) {
                    if node.kind() == Kind::ThisKeyword {
                        found.push(node);
                    } else if !ast::is_function_like(Some(node)) && !ast::is_class_like(node) {
                        node.for_each_child(&mut |child| {
                            cb(child, found);
                            false
                        });
                    }
                }
                let mut found: Vec<P<Node>> = Vec::new();
                cb(body, &mut found);
                for node in found {
                    self.add_to_group(add_ref, node, EntryKind::Node);
                }
            }
        }
    }

    // findallreferences.go:2352
    fn find_inherited_constructor_references(
        &mut self,
        class_declaration: P<Node>, /*ClassLikeDeclaration*/
    ) {
        if has_own_constructor(class_declaration) {
            return;
        }
        let class_symbol = class_declaration.symbol().unwrap();
        let search = self.create_search(None, class_symbol, ImpExpKind::Unknown, "", Vec::new());
        self.get_references_in_container_or_files(class_symbol, &search);
    }

    // findallreferences.go:2361
    fn get_import_or_export_references(
        &mut self,
        reference_location: P<Node>,
        reference_symbol: P<Symbol>,
        search: &RefSearch,
    ) {
        let import_or_export = get_import_or_export_symbol(
            reference_location,
            reference_symbol,
            self.checker,
            search.coming_from == ImpExpKind::Export,
        );
        let Some(import_or_export) = import_or_export else {
            return;
        };
        if import_or_export.kind == ImpExpKind::Import {
            if !is_for_rename_with_prefix_and_suffix_text(self.options) {
                self.search_for_imported_symbol(import_or_export.symbol.unwrap());
            }
        } else {
            self.search_for_imports_of_export(
                reference_location,
                import_or_export.symbol.unwrap(),
                import_or_export.export_info.unwrap(),
            );
        }
    }

    // findallreferences.go:2375
    fn mark_seen_re_export_rhs(&mut self, node: P<Node>) -> bool {
        self.seen_re_export_rhs.insert(node)
    }

    // findallreferences.go:2379
    fn get_references_at_export_specifier(
        &mut self,
        reference_location: P<Node>,
        reference_symbol: Option<P<Symbol>>,
        export_specifier: P<Node>, /*ExportSpecifier*/
        search: &RefSearch,
        add_references_here: bool,
        always_get_references: bool,
    ) {
        assert!(
            !always_get_references || self.options.use_aliases_for_rename,
            "If alwaysGetReferences is true, then prefix/suffix text must be enabled"
        );

        let export_declaration = export_specifier.parent().unwrap().parent().unwrap();
        let export_declaration_module_specifier =
            export_declaration.as_export_declaration().module_specifier;
        let property_name = export_specifier.property_name();
        let name = export_specifier.name().unwrap();
        let local_symbol = get_local_symbol_for_export_specifier(
            reference_location,
            reference_symbol,
            export_specifier,
            self.checker,
        );

        if !always_get_references && !search.includes(local_symbol) {
            return;
        }

        let add_ref = |state: &mut RefState| {
            if add_references_here {
                state.add_reference(reference_location, local_symbol.unwrap(), EntryKind::Node);
            }
        };

        match property_name {
            None => {
                // Don't rename at `export { default } from "m";`. (but do continue to search for imports of the re-export)
                if !(self.options.use_ == ReferenceUse::Rename
                    && ast::module_export_name_is_default(name))
                {
                    add_ref(self);
                }
            }
            Some(property_name) if reference_location == property_name => {
                // For `export { foo as bar } from "baz"`, "`foo`" will be added from the singleReferences for import searches of the original export.
                // For `export { foo as bar };`, where `foo` is a local, so add it now.
                if export_declaration_module_specifier.is_none() {
                    add_ref(self);
                }

                if add_references_here
                    && self.options.use_ != ReferenceUse::Rename
                    && self.mark_seen_re_export_rhs(name)
                {
                    let export_symbol = export_specifier.symbol();
                    assert!(export_symbol.is_some(), "exportSpecifier.Symbol() should not be nil");
                    self.add_reference(name, export_symbol.unwrap(), EntryKind::Node);
                }
            }
            Some(_) => {
                if self.mark_seen_re_export_rhs(reference_location) {
                    add_ref(self);
                }
            }
        }

        // For `export { foo as bar }`, rename `foo`, but not `bar`.
        if !is_for_rename_with_prefix_and_suffix_text(self.options) || always_get_references {
            let is_default_export = ast::module_export_name_is_default(reference_location)
                || ast::module_export_name_is_default(export_specifier.name().unwrap());
            let mut export_kind = ExportKind::Named;
            if is_default_export {
                export_kind = ExportKind::Default;
            }
            let export_symbol = export_specifier.symbol();
            assert!(export_symbol.is_some(), "exportSpecifier.Symbol() should not be nil");
            let export_symbol = export_symbol.unwrap();
            let export_info = get_export_info(export_symbol, export_kind, self.checker);
            if let Some(export_info) = export_info {
                self.search_for_imports_of_export(reference_location, export_symbol, export_info);
            }
        }

        // At `export { x } from "foo"`, also search for the imported symbol `"foo".x`.
        if search.coming_from != ImpExpKind::Export
            && export_declaration_module_specifier.is_some()
            && property_name.is_none()
            && !is_for_rename_with_prefix_and_suffix_text(self.options)
        {
            let imported = self.checker.get_export_specifier_local_target_symbol(export_specifier);
            if let Some(imported) = imported {
                self.search_for_imported_symbol(imported);
            }
        }
    }

    // Go to the symbol we imported from and find references for it.
    // findallreferences.go:2452
    fn search_for_imported_symbol(&mut self, symbol: P<Symbol>) {
        for &declaration in symbol.declarations() {
            let exporting_file = ast::get_source_file_of_node(declaration).unwrap();
            // Need to search in the file even if it's not in the search-file set, because it might export the symbol.
            let search =
                self.create_search(Some(declaration), symbol, ImpExpKind::Import, "", Vec::new());
            let add_references_here = self.includes_source_file(exporting_file);
            self.get_references_in_source_file(exporting_file, &search, add_references_here);
        }
    }

    // Search for all imports of a given exported symbol using `State.getImportSearches`. */
    // findallreferences.go:2461
    fn search_for_imports_of_export(
        &mut self,
        export_location: P<Node>,
        export_symbol: P<Symbol>,
        export_info: ExportInfo,
    ) {
        let r = self.get_import_searches(export_symbol, export_info);

        // For `import { foo as bar }` just add the reference to `foo`, and don't otherwise search in the file.
        if !r.single_references.is_empty() {
            let add_ref = self.reference_adder(export_symbol);
            for &single_ref in &r.single_references {
                if self.should_add_single_reference(single_ref) {
                    self.add_to_group(add_ref, single_ref, EntryKind::Node);
                }
            }
        }

        // For each import, find all references to that import in its source file.
        for i in &r.import_searches {
            let search = self.create_search(
                Some(i.import_location),
                i.import_symbol.unwrap(),
                ImpExpKind::Export,
                "",
                Vec::new(),
            );
            self.get_references_in_source_file(
                ast::get_source_file_of_node(i.import_location).unwrap(),
                &search,
                true, /*addReferencesHere*/
            );
        }

        if !r.indirect_users.is_empty() {
            let mut indirect_search: Option<RefSearch> = None;
            match export_info.export_kind {
                ExportKind::Named => {
                    indirect_search = Some(self.create_search(
                        Some(export_location),
                        export_symbol,
                        ImpExpKind::Export,
                        "",
                        Vec::new(),
                    ));
                }
                ExportKind::Default => {
                    // Search for a property access to '.default'. This can't be renamed.
                    if self.options.use_ != ReferenceUse::Rename {
                        indirect_search = Some(self.create_search(
                            Some(export_location),
                            export_symbol,
                            ImpExpKind::Export,
                            "default",
                            Vec::new(),
                        ));
                    }
                }
                ExportKind::ExportEquals => {}
            }
            if let Some(indirect_search) = indirect_search {
                for &indirect_user in &r.indirect_users {
                    self.search_for_name(indirect_user, &indirect_search);
                }
            }
        }
    }

    // findallreferences.go:2498
    fn should_add_single_reference(&self, single_ref: P<Node>) -> bool {
        if !self.has_matching_meaning(single_ref) {
            return false;
        }
        if self.options.use_ != ReferenceUse::Rename {
            return true;
        }
        // Don't rename an import type `import("./module-name")` when renaming `name` in `export = name;`
        if !ast::is_identifier(single_ref)
            && !ast::is_import_or_export_specifier(single_ref.parent().unwrap())
        {
            return false;
        }
        // At `default` in `import { default as x }` or `export { default as x }`, do add a reference, but do not rename.
        !(ast::is_import_or_export_specifier(single_ref.parent().unwrap())
            && ast::module_export_name_is_default(single_ref))
    }

    // findallreferences.go:2513
    fn has_matching_meaning(&self, reference_location: P<Node>) -> bool {
        get_meaning_from_location(reference_location).intersects(self.search_meaning)
    }

    // findallreferences.go:2517
    fn get_reference_for_shorthand_property(
        &mut self,
        reference_symbol: P<Symbol>,
        search: &RefSearch,
    ) {
        if reference_symbol.flags().intersects(SymbolFlags::Transient) {
            return;
        }
        let Some(value_declaration) = reference_symbol.value_declaration() else {
            return;
        };
        let shorthand_value_symbol =
            self.checker.get_shorthand_assignment_value_symbol(Some(value_declaration));
        let name = ast::get_name_of_declaration(Some(value_declaration));

        // Because in short-hand property assignment, an identifier which stored as name of the short-hand property assignment
        // has two meanings: property name and property value. Therefore when we do findAllReference at the position where
        // an identifier is declared, the language service should return the position of the variable declaration as well as
        // the position in short-hand property assignment excluding property accessing. However, if we do findAllReference at the
        // position of property accessing, the referenceEntry of such position will be handled in the first case.
        if let Some(name) = name
            && search.includes(shorthand_value_symbol)
        {
            self.add_reference(name, shorthand_value_symbol.unwrap(), EntryKind::Node);
        }
    }

    // === search ===
    // findallreferences.go:2535
    fn populate_search_symbol_set(
        &mut self,
        symbol: P<Symbol>,
        location: Option<P<Node>>,
        is_for_rename: bool,
        provide_prefix_and_suffix_text: bool,
        implementations: bool,
    ) -> Vec<P<Symbol>> {
        let Some(location) = location else {
            return vec![symbol];
        };
        let mut result: Vec<P<Symbol>> = Vec::new();
        self.for_each_related_symbol(
            symbol,
            location,
            is_for_rename,
            !(is_for_rename && provide_prefix_and_suffix_text),
            &mut |sym, root, base| {
                let mut base = base;
                // static method/property and instance method/property might have the same name. Only include static or only include instance.
                if let Some(b) = base
                    && is_static_symbol(symbol) != is_static_symbol(b)
                {
                    base = None;
                }
                result.push(base.or(root).unwrap_or(sym));
                None
            }, // when try to find implementation, implementations is true, and not allowed to find base class
            /*allowBaseTypes*/ &mut |_state, _| !implementations,
        );
        result
    }

    // findallreferences.go:2560
    fn get_related_symbol(
        &mut self,
        search: &RefSearch,
        reference_symbol: P<Symbol>,
        reference_location: P<Node>,
    ) -> (Option<P<Symbol>>, EntryKind) {
        let only_include_binding_element_at_reference_location =
            self.options.use_ != ReferenceUse::Rename || self.options.use_aliases_for_rename;
        self.for_each_related_symbol(
            reference_symbol,
            reference_location,
            false, /*isForRenamePopulateSearchSymbolSet*/
            only_include_binding_element_at_reference_location,
            &mut |sym, root_symbol, base_symbol| {
                let mut base_symbol = base_symbol;
                // check whether the symbol used to search itself is just the searched one.
                if let Some(b) = base_symbol {
                    // static method/property and instance method/property might have the same name. Only check static or only check instance.
                    if is_static_symbol(reference_symbol) != is_static_symbol(b) {
                        base_symbol = None;
                    }
                }
                let search_sym = base_symbol.or(root_symbol).unwrap_or(sym);
                if search.includes(Some(search_sym)) {
                    if root_symbol.is_some() && !sym.check_flags().intersects(CheckFlags::Synthetic)
                    {
                        return root_symbol;
                    }
                    return Some(sym);
                }
                // For a base type, use the symbol for the derived type. For a synthetic (e.g. union) property, use the union symbol.
                None
            },
            &mut |state, root_symbol| {
                !(!search.parents.is_empty()
                    && !search.parents.iter().any(|&parent| {
                        state.explicitly_inherits_from(root_symbol.parent().unwrap(), parent)
                    }))
            },
        )
    }

    // Go's `fromRoot` closure; it receives the state and the callbacks first (PORTING.md callback rule).
    // findallreferences.go:2600
    #[expect(
        clippy::wrong_self_convention,
        reason = "The method name and mutable receiver match the upstream Go operation."
    )]
    fn from_root(
        &mut self,
        cb_symbol: &mut dyn FnMut(
            P<Symbol>,
            Option<P<Symbol>>,
            Option<P<Symbol>>,
        ) -> Option<P<Symbol>>,
        allow_base_types: &mut dyn FnMut(&mut RefState, P<Symbol>) -> bool,
        sym: P<Symbol>,
    ) -> Option<P<Symbol>> {
        // If this is a union property:
        //   - In populateSearchSymbolsSet we will add all the symbols from all its source symbols in all unioned types.
        //   - In findRelatedSymbol, we will just use the union symbol if any source symbol is included in the search.
        // If the symbol is an instantiation from a another symbol (e.g. widened symbol):
        //   - In populateSearchSymbolsSet, add the root the list
        //   - In findRelatedSymbol, return the source symbol if that is in the search. (Do not return the instantiation symbol.)
        if let Some(result) =
            self.checker.get_root_symbols(sym).into_iter().find_map(|root_symbol| {
                if let Some(result) = cb_symbol(sym, Some(root_symbol), None /*baseSymbol*/) {
                    return Some(Some(result));
                }
                // Add symbol of properties/methods of the same name in base classes and implemented interfaces definitions
                if let Some(root_parent) = root_symbol.parent()
                    && root_parent.flags().intersects(SymbolFlags::Class | SymbolFlags::Interface)
                    && allow_base_types(self, root_symbol)
                {
                    let result = get_property_symbols_from_base_types(
                        root_parent,
                        root_symbol.name(),
                        self.checker,
                        &mut |_c, base| cb_symbol(sym, Some(root_symbol), Some(base)),
                    );
                    if result.is_some() {
                        return Some(result);
                    }
                }

                None
            })
        {
            return result;
        }
        None
    }

    // findallreferences.go:2592
    fn for_each_related_symbol(
        &mut self,
        symbol: P<Symbol>,
        location: P<Node>,
        is_for_rename_populate_search_symbol_set: bool,
        only_include_binding_element_at_reference_location: bool,
        cb_symbol: &mut dyn FnMut(
            P<Symbol>,
            Option<P<Symbol>>,
            Option<P<Symbol>>,
        ) -> Option<P<Symbol>>,
        allow_base_types: &mut dyn FnMut(&mut RefState, P<Symbol>) -> bool,
    ) -> (Option<P<Symbol>>, EntryKind) {
        if let Some(containing_object_literal_element) =
            get_containing_object_literal_element(location)
        {
            /* Because in short-hand property assignment, location has two meaning : property name and as value of the property
             * When we do findAllReference at the position of the short-hand property assignment, we would want to have references to position of
             * property name and variable declaration of the identifier.
             * Like in below example, when querying for all references for an identifier 'name', of the property assignment, the language service
             * should show both 'name' in 'obj' and 'name' in variable declaration
             *      const name = "Foo";
             *      const obj = { name };
             * In order to do that, we will populate the search set with the value symbol of the identifier as a value of the property assignment
             * so that when matching with potential reference symbol, both symbols from property declaration and variable declaration
             * will be included correctly.
             */
            let shorthand_value_symbol =
                self.checker.get_shorthand_assignment_value_symbol(location.parent());
            // gets the local symbol
            if let Some(shorthand_value_symbol) = shorthand_value_symbol
                && is_for_rename_populate_search_symbol_set
            {
                // When renaming 'x' in `const o = { x }`, just rename the local variable, not the property.
                return (
                    cb_symbol(
                        shorthand_value_symbol,
                        None, /*rootSymbol*/
                        None, /*baseSymbol*/
                    ),
                    EntryKind::SearchedLocalFoundProperty,
                );
            }
            // If the location is in a context sensitive location (i.e. in an object literal) try
            // to get a contextual type for it, and add the property symbol from the contextual
            // type to the search set
            if let Some(contextual_type) = self.checker.get_contextual_type_exported(
                containing_object_literal_element.parent().unwrap(),
                ContextFlags::None,
            ) {
                let symbols = self.checker.get_property_symbols_from_contextual_type(
                    containing_object_literal_element,
                    contextual_type,
                    true, /*unionSymbolOk*/
                );
                if let Some(result) = symbols.into_iter().find_map(|sym| {
                    if let Some(res) = self.from_root(cb_symbol, allow_base_types, sym) {
                        return Some((Some(res), EntryKind::SearchedPropertyFoundLocal));
                    }

                    None
                }) {
                    return result;
                }
            }
            // If the location is name of property symbol from object literal destructuring pattern
            // Search the property symbol
            //      for ( { property: p2 } of elems) { }
            if let Some(property_symbol) =
                self.checker.get_property_symbol_of_destructuring_assignment(location)
                && let Some(res) =
                    cb_symbol(property_symbol, None /*rootSymbol*/, None /*baseSymbol*/)
            {
                return (Some(res), EntryKind::SearchedPropertyFoundLocal);
            }
            if let Some(shorthand_value_symbol) = shorthand_value_symbol
                && let Some(res) = cb_symbol(
                    shorthand_value_symbol,
                    None, /*rootSymbol*/
                    None, /*baseSymbol*/
                )
            {
                return (Some(res), EntryKind::SearchedLocalFoundProperty);
            }
        }

        if let Some(aliased_symbol) = get_merged_aliased_symbol_of_namespace_export_declaration(
            location,
            symbol,
            self.checker,
        ) {
            // In case of UMD module and global merging, search for global as well
            if let Some(res) =
                cb_symbol(aliased_symbol, None /*rootSymbol*/, None /*baseSymbol*/)
            {
                return (Some(res), EntryKind::Node);
            }
        }

        if let Some(res) = self.from_root(cb_symbol, allow_base_types, symbol) {
            return (Some(res), EntryKind::Node);
        }

        if let Some(value_declaration) = symbol.value_declaration() {
            // (Go passes a possibly nil parent; IsParameterPropertyDeclaration reads it only for parameters, which have one.)
            if value_declaration.parent().is_some_and(|parent| {
                ast::is_parameter_property_declaration(value_declaration, parent)
            }) {
                let (param_prop1, param_prop2) =
                    self.checker.get_symbols_of_parameter_property_declaration(
                        value_declaration,
                        symbol.name(),
                    );
                assert!(
                    param_prop1.flags().intersects(SymbolFlags::FunctionScopedVariable)
                        && param_prop2.flags().intersects(SymbolFlags::ClassMember),
                    "GetSymbolsOfParameterPropertyDeclaration must return (parameter, member) pair"
                );
                let sym = if symbol.flags().intersects(SymbolFlags::FunctionScopedVariable) {
                    param_prop2
                } else {
                    param_prop1
                };
                return (self.from_root(cb_symbol, allow_base_types, sym), EntryKind::Node);
            }
        }

        if let Some(export_specifier) = ast::get_declaration_of_kind(symbol, Kind::ExportSpecifier)
            && (!is_for_rename_populate_search_symbol_set
                || export_specifier.property_name().is_none())
            && let Some(local_symbol) =
                self.checker.get_export_specifier_local_target_symbol(export_specifier)
            && let Some(res) =
                cb_symbol(local_symbol, None /*rootSymbol*/, None /*baseSymbol*/)
        {
            return (Some(res), EntryKind::Node);
        }

        // symbolAtLocation for a binding element is the local symbol. See if the search symbol is the property.
        // Don't do this when populating search set for a rename when prefix and suffix text will be provided -- just rename the local.
        if !is_for_rename_populate_search_symbol_set {
            let binding_element_property_symbol: Option<P<Symbol>> =
                if only_include_binding_element_at_reference_location {
                    if !is_object_binding_element_without_property_name(location.parent().unwrap())
                    {
                        return (None, EntryKind::None);
                    }
                    get_property_symbol_from_binding_element(
                        self.checker,
                        location.parent().unwrap(),
                    )
                } else {
                    get_property_symbol_of_object_binding_pattern_without_property_name(
                        symbol,
                        self.checker,
                    )
                };
            let Some(binding_element_property_symbol) = binding_element_property_symbol else {
                return (None, EntryKind::None);
            };
            return (
                self.from_root(cb_symbol, allow_base_types, binding_element_property_symbol),
                EntryKind::SearchedPropertyFoundLocal,
            );
        }

        assert!(is_for_rename_populate_search_symbol_set);

        // due to the above assert and the arguments at the uses of this function,
        // (onlyIncludeBindingElementAtReferenceLocation <=> !providePrefixAndSuffixTextForRename) holds
        let include_original_symbol_of_binding_element =
            only_include_binding_element_at_reference_location;

        if include_original_symbol_of_binding_element
            && let Some(binding_element_property_symbol) =
                get_property_symbol_of_object_binding_pattern_without_property_name(
                    symbol,
                    self.checker,
                )
        {
            return (
                self.from_root(cb_symbol, allow_base_types, binding_element_property_symbol),
                EntryKind::SearchedPropertyFoundLocal,
            );
        }
        (None, EntryKind::None)
    }

    // Search for all occurrences of an identifier in a source file (and filter out the ones that match).
    // findallreferences.go:2729
    fn search_for_name(&mut self, source_file: P<SourceFile>, search: &RefSearch) {
        if source_file.get_name_table().contains_key(search.escaped_text.as_str()) {
            self.get_references_in_source_file(
                source_file,
                search,
                true, /*addReferencesHere*/
            );
        }
    }

    // findallreferences.go:2735
    fn explicitly_inherits_from(&mut self, symbol: P<Symbol>, parent: P<Symbol>) -> bool {
        if symbol == parent {
            return true;
        }

        // Check cache first
        let key = InheritKey { symbol, parent };
        if let Some(&cached) = self.inherits_from_cache.get(&key) {
            return cached;
        }

        // Set to false initially to prevent infinite recursion
        self.inherits_from_cache.insert(key, false);

        if symbol.declarations().is_empty() {
            return false;
        }

        let inherits = symbol.declarations().iter().any(|&declaration| {
            let super_type_nodes = get_all_super_type_nodes(declaration);
            super_type_nodes.iter().any(|&type_reference| {
                let typ = self.checker.get_type_at_location(type_reference);
                typ.symbol()
                    .is_some_and(|typ_symbol| self.explicitly_inherits_from(typ_symbol, parent))
            })
        });

        // Update cache with the actual result
        self.inherits_from_cache.insert(key, inherits);
        inherits
    }
}
