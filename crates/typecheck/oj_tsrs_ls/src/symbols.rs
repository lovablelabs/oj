// Port of ls/symbols.go.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{self as ast, JSDeclarationKind, Kind, ModifierFlags, Node, NodeFlags, SourceFile};
use tsrs_compiler::Program;
use tsrs_core::collections::MultiMap;
use tsrs_core::context::Context;
use tsrs_core::goslices;
use tsrs_core::stringutil;
use tsrs_core::tspath::Path;
use tsrs_core::{P, TextRange};
use tsrs_lsproto as lsproto;
use tsrs_printer as printer;
use tsrs_scanner as scanner;

use crate::astnav;
use crate::completions_2::str_ptr_to;
use crate::languageservice::LanguageService;
use crate::lsconv::Converters;
use crate::lsutil::UserPreferences;
use crate::spanmap::Feature;
use crate::utilities::get_container_node;

// Go builds `[]*lsproto.DocumentSymbol` and `mergeExpandos` / `mergeChildren` mutate the pointed-to symbols, which
// may be shared between several parents' child lists; the shared-pointer tree keeps those semantics and is turned
// into `lsproto::DocumentSymbol` values when the response is built.
pub type DocSym = Rc<RefCell<DocSymData>>;

pub struct DocSymData {
    name: String,
    kind: lsproto::SymbolKind,
    range: lsproto::Range,
    selection_range: lsproto::Range,
    children: Option<Vec<DocSym>>,
}

pub fn doc_sym_to_lsp(symbol: &DocSym) -> lsproto::DocumentSymbol {
    let s = symbol.borrow();
    lsproto::DocumentSymbol {
        name: s.name.clone(),
        kind: s.kind,
        range: s.range,
        selection_range: s.selection_range,
        children: s.children.as_ref().map(|children| children.iter().map(doc_sym_to_lsp).collect()),
        ..Default::default()
    }
}

impl LanguageService {
    // symbols.go:25
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    pub fn provide_document_symbols(
        &self,
        ctx: &Context,
        document_uri: &lsproto::DocumentUri,
    ) -> Result<lsproto::DocumentSymbolResponse, lsproto::Error> {
        let (_, file) = self.get_program_and_file(document_uri);
        let mut projections = vec![file];
        projections.extend_from_slice(file.supplemental_source_files());
        let mut symbols: Vec<lsproto::DocumentSymbol> = Vec::new();
        let mut seen: FxHashSet<(String, lsproto::SymbolKind, lsproto::Range)> =
            FxHashSet::default();
        symbols.extend(
            projections
                .into_iter()
                .flat_map(|projection| {
                    self.get_document_symbols_for_children(ctx, projection.as_node(), projection)
                })
                .filter_map(|symbol| {
                    let symbol = doc_sym_to_lsp(&symbol);
                    let key = (symbol.name.clone(), symbol.kind, symbol.range);
                    seen.insert(key).then_some(symbol)
                }),
        );
        if lsproto::get_client_capabilities(ctx)
            .text_document
            .document_symbol
            .hierarchical_document_symbol_support
        {
            return Ok(lsproto::SymbolInformationsOrDocumentSymbolsOrNull {
                document_symbols: Some(symbols),
                ..Default::default()
            });
        }
        // Client doesn't support hierarchical document symbols, return flat SymbolInformation array
        let symbol_infos = flatten_document_symbols(&symbols, document_uri);
        Ok(lsproto::SymbolInformationsOrDocumentSymbolsOrNull {
            symbol_informations: Some(symbol_infos),
            ..Default::default()
        })
    }
}

// symbols.go:65
fn flatten_document_symbols(
    doc_symbols: &[lsproto::DocumentSymbol],
    document_uri: &lsproto::DocumentUri,
) -> Vec<lsproto::SymbolInformation> {
    // Flatten the hierarchy
    fn flatten(
        result: &mut Vec<lsproto::SymbolInformation>,
        symbols: &[lsproto::DocumentSymbol],
        container_name: Option<&String>,
        document_uri: &lsproto::DocumentUri,
    ) {
        for symbol in symbols {
            let info = lsproto::SymbolInformation {
                name: symbol.name.clone(),
                kind: symbol.kind,
                location: lsproto::Location { uri: document_uri.clone(), range: symbol.range },
                container_name: container_name.cloned(),
                tags: symbol.tags.clone(),
                deprecated: symbol.deprecated,
            };
            result.push(info);

            // Recursively flatten children with this symbol as container
            if let Some(children) = &symbol.children
                && !children.is_empty()
            {
                flatten(result, children, Some(&symbol.name), document_uri);
            }
        }
    }
    let mut result = Vec::new();
    flatten(&mut result, doc_symbols, None, document_uri);
    result
}

// The closures of getDocumentSymbolsForChildren share `symbols` and `expandoTargets`.
struct DocumentSymbolsVisitor<'a> {
    ls: &'a LanguageService,
    ctx: &'a Context,
    file: P<SourceFile>,
    symbols: Vec<DocSym>,
    expando_targets: FxHashSet<String>,
}

impl DocumentSymbolsVisitor<'_> {
    fn add_symbol_for_node(&mut self, node: P<Node>, name: Option<P<Node>>, children: Vec<DocSym>) {
        if !node.flags().intersects(NodeFlags::Reparsed)
            && let Some(symbol) = self.ls.new_document_symbol(node, name, children)
        {
            self.symbols.push(symbol);
        }
    }

    fn get_symbols_for_children(&mut self, node: Option<P<Node>>) -> Vec<DocSym> {
        let mut result = Vec::new();
        if let Some(node) = node {
            let save_expando_targets = std::mem::take(&mut self.expando_targets);
            let save_symbols = std::mem::take(&mut self.symbols);
            node.for_each_child(&mut |c| self.visit(c));
            result = std::mem::replace(&mut self.symbols, save_symbols);
            self.expando_targets = save_expando_targets;
        }
        result
    }

    // Go returns the closure that ends the node; `start_node` returns the saved state and `end_node` restores it.
    fn start_node(&mut self) -> (Vec<DocSym>, FxHashSet<String>) {
        let save_expando_targets = std::mem::take(&mut self.expando_targets);
        let save_symbols = std::mem::take(&mut self.symbols);
        (save_symbols, save_expando_targets)
    }

    fn end_node(
        &mut self,
        saved: (Vec<DocSym>, FxHashSet<String>),
        node: P<Node>,
        name: Option<P<Node>>,
    ) {
        let (save_symbols, save_expando_targets) = saved;
        let result = std::mem::replace(&mut self.symbols, save_symbols);
        self.expando_targets = save_expando_targets;
        self.add_symbol_for_node(node, name, result);
    }

    fn get_symbols_for_node(&mut self, node: Option<P<Node>>) -> Vec<DocSym> {
        let mut result = Vec::new();
        if let Some(node) = node {
            let save_symbols = std::mem::take(&mut self.symbols);
            self.visit(node);
            result = std::mem::replace(&mut self.symbols, save_symbols);
        }
        result
    }

    fn visit(&mut self, node: P<Node>) -> bool {
        if self.ctx.err().is_some() {
            return true;
        }
        if !node.flags().intersects(NodeFlags::Reparsed) {
            let jsdocs = node.jsdoc(Some(self.file.get()));
            for &jsdoc in jsdocs {
                if let Some(tag_list) = jsdoc.as_jsdoc().tags {
                    for &tag in tag_list.nodes() {
                        if ast::is_jsdoc_typedef_tag(tag) || ast::is_jsdoc_callback_tag(tag) {
                            self.add_symbol_for_node(
                                tag,
                                None,       /*name*/
                                Vec::new(), /*children*/
                            );
                        }
                    }
                }
            }
        }
        if node.parent().is_some_and(|parent| parent.kind() == Kind::SourceFile)
            && ast::is_import_or_import_equals_declaration(node)
        {
            return false;
        }
        match node.kind() {
            Kind::ClassDeclaration
            | Kind::ClassExpression
            | Kind::InterfaceDeclaration
            | Kind::EnumDeclaration => {
                if ast::is_class_like(node) && !ast::get_declaration_name(node).is_empty() {
                    self.expando_targets.insert(ast::get_declaration_name(node));
                }
                let children = self.get_symbols_for_children(Some(node));
                self.add_symbol_for_node(node, None /*name*/, children);
            }
            Kind::ModuleDeclaration => {
                let children = self.get_symbols_for_children(Some(get_interior_module(node)));
                self.add_symbol_for_node(node, None /*name*/, children);
            }
            Kind::Constructor => {
                let children = self.get_symbols_for_children(node.body());
                self.add_symbol_for_node(node, None /*name*/, children);
                for &param in node.parameters() {
                    if ast::is_parameter_property_declaration(param, node) {
                        self.add_symbol_for_node(
                            param,
                            None,       /*name*/
                            Vec::new(), /*children*/
                        );
                    }
                }
            }
            Kind::FunctionDeclaration
            | Kind::FunctionExpression
            | Kind::ArrowFunction
            | Kind::MethodDeclaration
            | Kind::GetAccessor
            | Kind::SetAccessor => {
                let decl_name = ast::get_declaration_name(node);
                if !decl_name.is_empty() {
                    self.expando_targets.insert(decl_name);
                }
                let children = self.get_symbols_for_children(node.body());
                self.add_symbol_for_node(node, None /*name*/, children);
            }
            Kind::VariableDeclaration
            | Kind::BindingElement
            | Kind::PropertyAssignment
            | Kind::PropertyDeclaration => {
                if let Some(node_name) = node.name() {
                    if ast::is_binding_pattern(node_name) {
                        self.visit(node_name);
                    } else {
                        let children = self.get_symbols_for_children(node.initializer());
                        self.add_symbol_for_node(node, None /*name*/, children);
                    }
                }
            }
            Kind::SpreadAssignment => {
                self.add_symbol_for_node(node, node.expression(), Vec::new() /*children*/);
            }
            Kind::MethodSignature
            | Kind::PropertySignature
            | Kind::CallSignature
            | Kind::ConstructSignature
            | Kind::IndexSignature
            | Kind::EnumMember
            | Kind::ShorthandPropertyAssignment
            | Kind::TypeAliasDeclaration
            | Kind::ImportEqualsDeclaration
            | Kind::ExportSpecifier => {
                self.add_symbol_for_node(node, None /*name*/, Vec::new() /*children*/);
            }
            Kind::ImportClause => {
                // Handle default import case e.g.:
                //    import d from "mod";
                if let Some(name) = node.name() {
                    self.add_symbol_for_node(name, Some(name), Vec::new() /*children*/);
                }
                // Handle named bindings in imports e.g.:
                //    import * as NS from "mod";
                //    import {a, b as B} from "mod";
                if let Some(named_bindings) = node.as_import_clause().named_bindings {
                    if named_bindings.kind() == Kind::NamespaceImport {
                        self.add_symbol_for_node(
                            named_bindings,
                            None,       /*name*/
                            Vec::new(), /*children*/
                        );
                    } else {
                        for &element in named_bindings.elements() {
                            self.add_symbol_for_node(
                                element,
                                None,       /*name*/
                                Vec::new(), /*children*/
                            );
                        }
                    }
                }
            }
            Kind::BinaryExpression | Kind::CallExpression => {
                let assignment_kind = ast::get_assignment_declaration_kind(node);
                match assignment_kind {
                    // `module.exports = ...`` should be reparsed into a JSExportAssignment,
                    // and `exports.a = ...`` into a CommonJSExport.
                    JSDeclarationKind::None
                    | JSDeclarationKind::ThisProperty
                    | JSDeclarationKind::ModuleExports
                    | JSDeclarationKind::ExportsProperty
                    | JSDeclarationKind::ObjectDefinePropertyExports => {
                        node.for_each_child(&mut |c| self.visit(c));
                    }
                    JSDeclarationKind::Property | JSDeclarationKind::ObjectDefinePropertyValue => {
                        let target: P<Node>;
                        let mut target_function: P<Node>;
                        let definition: P<Node>;
                        let property_name: Option<P<Node>>;
                        // `A.b = ... ` or `A.prototype.b = ...`
                        if ast::is_binary_expression(node) {
                            let binary_expr = node.as_binary_expression();
                            target = binary_expr.left;
                            target_function = target.expression().unwrap();
                            definition = binary_expr.right.get();
                            // `A.b` or `A.prototype.b`
                            if ast::is_property_access_expression(target) {
                                property_name = target.name();
                            } else {
                                // `A["b"]` or `A.prototype["b"]`
                                property_name =
                                    Some(target.as_element_access_expression().argument_expression);
                            }
                        } else {
                            // `Object.defineProperty(A, "b", {...})`
                            let args = node.arguments();
                            target_function = args[0];
                            target = args[1];
                            property_name = Some(target);
                            definition = args[2];
                        }
                        if is_prototype_expando(target_function) {
                            target_function = target_function.expression().unwrap();
                            // If we see a prototype assignment, start tracking the target as an expando target.
                            if ast::is_identifier(target_function) {
                                self.expando_targets.insert(target_function.text().to_string());
                            }
                        }
                        if ast::is_identifier(target_function)
                            && self.expando_targets.contains(target_function.text())
                        {
                            let saved = self.start_node();
                            let children = self.get_symbols_for_node(Some(definition));
                            self.add_symbol_for_node(target, property_name, children);
                            self.end_node(saved, node, Some(target_function));
                        } else {
                            node.for_each_child(&mut |c| self.visit(c));
                        }
                    }
                }
            }
            Kind::ExportAssignment => {
                if node.as_export_assignment().is_export_equals {
                    let children = self.get_symbols_for_node(node.expression());
                    self.add_symbol_for_node(node, None /*name*/, children);
                } else {
                    node.for_each_child(&mut |c| self.visit(c));
                }
            }
            _ => {
                node.for_each_child(&mut |c| self.visit(c));
            }
        }
        false
    }
}

impl LanguageService {
    // symbols.go:94
    fn get_document_symbols_for_children(
        &self,
        ctx: &Context,
        node: P<Node>,
        file: P<SourceFile>,
    ) -> Vec<DocSym> {
        let mut v = DocumentSymbolsVisitor {
            ls: self,
            ctx,
            file,
            symbols: Vec::new(),
            expando_targets: FxHashSet::default(),
        };
        node.for_each_child(&mut |c| v.visit(c));
        merge_expandos(v.symbols)
    }
}

// Target is `f.prototype`.
// symbols.go:282
fn is_prototype_expando(target: P<Node>) -> bool {
    if ast::is_access_expression(target) {
        let access_name = ast::get_element_or_property_access_name(target);
        return access_name.is_some_and(|n| n.text() == "prototype");
    }
    false
}

// symbols.go:290
const MAX_LENGTH: usize = 150;

impl LanguageService {
    // symbols.go:292
    fn new_document_symbol(
        &self,
        node: P<Node>,
        name: Option<P<Node>>,
        children: Vec<DocSym>,
    ) -> Option<DocSym> {
        let file = ast::get_source_file_of_node(node).unwrap();
        let node_start_pos = scanner::skip_trivia(
            file.text(),
            node.pos().expect_source("document-symbol nodes have source positions"),
        );
        let name = if name.is_none() { ast::get_name_of_declaration(node) } else { name };
        let mut text: String;
        let name_start_pos: u32;
        let name_end_pos: u32;
        if ast::is_module_declaration(node) && !ast::is_ambient_module(node) {
            text = get_module_name(node);
            name_start_pos = scanner::skip_trivia(
                file.text(),
                name.unwrap().pos().expect_source("module names have source positions"),
            );
            name_end_pos = get_interior_module(node)
                .name()
                .unwrap()
                .end()
                .expect_source("module names have source positions");
        } else if ast::is_any_export_assignment(node)
            && node.as_export_assignment().is_export_equals
        {
            text = "export=".to_string();
            if ast::node_is_missing(name) {
                name_start_pos = node_start_pos;
                name_end_pos =
                    node.end().expect_source("document-symbol nodes have source positions");
            } else {
                name_start_pos = scanner::skip_trivia(
                    file.text(),
                    name.unwrap().pos().expect_source("export names have source positions"),
                );
                name_end_pos =
                    name.unwrap().end().expect_source("export names have source positions");
            }
        } else if let Some(name) = name {
            text = get_text_of_name(name);
            name_start_pos = scanner::skip_trivia(
                file.text(),
                name.pos().expect_source("declaration names have source positions"),
            )
            .max(node_start_pos);
            name_end_pos = name
                .end()
                .expect_source("declaration names have source positions")
                .max(node_start_pos);
        } else {
            text = get_unnamed_node_label(node);
            name_start_pos = node_start_pos;
            name_end_pos = node_start_pos;
        }
        if text.is_empty() {
            return None;
        }
        let truncated_text = stringutil::truncate_by_runes(&text, MAX_LENGTH);
        if truncated_text.len() < text.len() {
            text = format!("{truncated_text}...");
        }
        let kind = get_symbol_kind_from_node(node);
        let (selection_range, selection_fidelity) = self.converters.to_lsp_range_for_feature(
            &file,
            TextRange::new(name_start_pos, name_end_pos),
            Feature::DocumentSymbols,
        );
        if !selection_fidelity.is_single_segment() {
            return None;
        }
        let (mut symbol_range, range_fidelity) = self.converters.to_lsp_range_for_feature(
            &file,
            TextRange::new(
                node_start_pos,
                node.end().expect_source("document-symbol nodes have source positions"),
            ),
            Feature::DocumentSymbols,
        );
        if range_fidelity.is_none() {
            symbol_range = selection_range;
        }
        Some(Rc::new(RefCell::new(DocSymData {
            name: text,
            kind,
            range: symbol_range,
            selection_range,
            children: Some(children),
        })))
    }
}

// Merges expando symbols into their target symbols, and namespaces of same name.
// Modifies the input slice.
// symbols.go:351
fn merge_expandos(symbols: Vec<DocSym>) -> Vec<DocSym> {
    let mut symbols: Vec<Option<DocSym>> = symbols.into_iter().map(Some).collect();
    let mut merged_symbols = Vec::with_capacity(symbols.len());
    // Collect symbols that can be an expando target.
    let mut name_to_expando_target_index: MultiMap<String, usize> = MultiMap::default();
    // Collect namespaces.
    let mut name_to_namespace_index: FxHashMap<String, usize> = FxHashMap::default();
    for (i, symbol) in symbols.iter().enumerate() {
        let symbol = symbol.as_ref().unwrap().borrow();
        if is_anonymous_name(&symbol.name) {
            continue;
        }
        if symbol.kind == lsproto::SymbolKind::CLASS
            || symbol.kind == lsproto::SymbolKind::FUNCTION
            || symbol.kind == lsproto::SymbolKind::VARIABLE
        {
            name_to_expando_target_index.add(symbol.name.clone(), i);
        }
        if symbol.kind == lsproto::SymbolKind::NAMESPACE {
            name_to_namespace_index.entry(symbol.name.clone()).or_insert(i);
        }
    }
    for i in 0..symbols.len() {
        // Go ranges over the slice, reading symbols[i] at each step (an earlier step only clears its own index).
        let Some(symbol) = symbols[i].clone() else {
            continue;
        };
        let children = symbol.borrow_mut().children.take();
        if let Some(children) = children {
            let children = merge_expandos(children);
            symbol.borrow_mut().children = Some(children);
        }

        let (name, kind) = {
            let s = symbol.borrow();
            (s.name.clone(), s.kind)
        };
        // Anonymous symbols never merge.
        if is_anonymous_name(&name) {
            continue;
        }

        // Merge expandos.
        if kind == lsproto::SymbolKind::PROPERTY {
            let symbols_with_same_name = name_to_expando_target_index.get(&name).to_vec();
            for &target_index in symbols_with_same_name.iter().rev() {
                // Go reads symbols[targetIndex], which a namespace merge may have set to nil (never a
                // class / function / variable target).
                let target_symbol = symbols[target_index].clone().unwrap();
                merge_children(&target_symbol, &symbol);
                // Mark this symbol as merged.
                symbols[i] = None;
            }
        }
        // Merge namespaces.
        if kind == lsproto::SymbolKind::NAMESPACE
            && let Some(&target_index) = name_to_namespace_index.get(&name)
            && target_index != i
        {
            let target_symbol = symbols[target_index].clone().unwrap();
            merge_children(&target_symbol, &symbol);
            // Mark this symbol as merged.
            symbols[i] = None;
        }
    }
    merged_symbols.extend(symbols.into_iter().flatten());
    merged_symbols
}

// symbols.go:410
fn merge_children(target: &DocSym, source: &DocSym) {
    let source_children = source.borrow().children.clone();
    if let Some(source_children) = source_children {
        let target_children = target.borrow_mut().children.take();
        match target_children {
            None => {
                target.borrow_mut().children = Some(source_children);
            }
            Some(mut target_children) => {
                target_children.extend(source_children);
                let mut merged = merge_expandos(target_children);
                goslices::sort_func(&mut merged, |a, b| {
                    lsproto::compare_ranges(a.borrow().range, b.borrow().range)
                });
                target.borrow_mut().children = Some(merged);
            }
        }
    }
}

// See `getUnnamedNodeLabel`.
// symbols.go:424
fn is_anonymous_name(name: &str) -> bool {
    name == "<function>"
        || name == "<class>"
        || name == "export="
        || name == "default"
        || name == "constructor"
        || name == "()"
        || name == "new()"
        || name == "[]"
        || name.ends_with(") callback")
}

// symbols.go:429
fn get_text_of_name(node: P<Node>) -> String {
    match node.kind() {
        Kind::Identifier | Kind::PrivateIdentifier | Kind::NumericLiteral => {
            return node.text().to_string();
        }
        Kind::StringLiteral => {
            return format!(
                "\"{}\"",
                printer::escape_string(node.text(), printer::QuoteChar::DoubleQuote)
            );
        }
        Kind::NoSubstitutionTemplateLiteral => {
            return format!(
                "`{}`",
                printer::escape_string(node.text(), printer::QuoteChar::Backtick)
            );
        }
        Kind::ComputedPropertyName
            if ast::is_string_or_numeric_literal_like(node.expression().unwrap()) =>
        {
            return get_text_of_name(node.expression().unwrap());
        }
        _ => {}
    }
    scanner::get_text_of_node(node)
}

// symbols.go:445
fn get_unnamed_node_label(node: P<Node>) -> String {
    if let Some(parent) = ast::walk_up_parenthesized_expressions(node.parent())
        && ast::is_export_assignment(parent)
    {
        if parent.as_export_assignment().is_export_equals {
            return "export=".to_string();
        }
        return "default".to_string();
    }
    match node.kind() {
        Kind::FunctionDeclaration | Kind::FunctionExpression | Kind::ArrowFunction => {
            if node.modifier_flags().intersects(ModifierFlags::Default) {
                return "default".to_string();
            }
            let parent = node.parent().unwrap();
            if ast::is_call_expression(parent) {
                let name = get_call_expression_name(parent.expression().unwrap());
                if !name.is_empty() {
                    let name = clean_callback_text(&name);
                    if name.len() > MAX_LENGTH {
                        return name + " callback";
                    }
                    let args = clean_callback_text(&get_call_expression_literal_args(parent));
                    return name + "(" + &args + ") callback";
                }
            }
            "<function>".to_string()
        }
        Kind::ClassDeclaration | Kind::ClassExpression => {
            if node.modifier_flags().intersects(ModifierFlags::Default) {
                return "default".to_string();
            }
            "<class>".to_string()
        }
        Kind::Constructor => "constructor".to_string(),
        Kind::CallSignature => "()".to_string(),
        Kind::ConstructSignature => "new()".to_string(),
        Kind::IndexSignature => "[]".to_string(),
        _ => String::new(),
    }
}

// symbols.go:486
fn get_call_expression_name(node: P<Node>) -> String {
    match node.kind() {
        Kind::Identifier | Kind::PrivateIdentifier => node.text().to_string(),
        Kind::PropertyAccessExpression => {
            let left = get_call_expression_name(node.expression().unwrap());
            let right = get_call_expression_name(node.name().unwrap());
            if !left.is_empty() {
                return left + "." + &right;
            }
            right
        }
        _ => String::new(),
    }
}

// symbols.go:501
fn get_call_expression_literal_args(call_expr: P<Node>) -> String {
    let mut parts = Vec::new();
    parts.extend(call_expr.arguments().iter().filter_map(|&arg| {
        if ast::is_string_literal_like(arg) || ast::is_template_expression(arg) {
            Some(scanner::get_text_of_node(arg))
        } else {
            None
        }
    }));
    parts.join(", ")
}

// symbols.go:511
fn clean_callback_text(text: &str) -> String {
    let mut text = text.to_string();
    let truncated = stringutil::truncate_by_runes(&text, MAX_LENGTH);
    if truncated.len() < text.len() {
        text = format!("{truncated}...");
    }
    text.chars().filter(|&r| !stringutil::is_line_break(r as u32)).collect()
}

// symbols.go:524
pub fn get_interior_module(node: P<Node>) -> P<Node> {
    let mut node = node;
    while let Some(body) = node.body().filter(|b| ast::is_module_declaration(*b)) {
        node = body;
    }
    node
}

// symbols.go:531
fn get_module_name(node: P<Node>) -> String {
    let mut node = node;
    let mut result = node.name().unwrap().text().to_string();
    while let Some(body) = node.body().filter(|b| ast::is_module_declaration(*b)) {
        node = body;
        result = result + "." + node.name().unwrap().text();
    }
    result
}

// symbols.go:540
struct DeclarationInfo {
    name: String,
    declaration: P<Node>,
    match_score: usize,
}

// symbols.go:546
#[expect(
    clippy::unnecessary_wraps,
    reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
)]
pub fn provide_workspace_symbols(
    ctx: &Context,
    programs: &[&'static Program],
    converters: &Converters,
    preferences: &UserPreferences,
    query: &str,
) -> Result<lsproto::WorkspaceSymbolResponse, lsproto::Error> {
    let exclude_library_symbols = preferences.exclude_library_symbols_in_nav_to.is_true();
    // Obtain set of non-declaration source files from all active programs.
    // (Go iterates a map in random order; the matches are sorted below.)
    let mut source_files: FxHashMap<Path, P<SourceFile>> = FxHashMap::default();
    for program in programs {
        source_files.extend(program.source_files().iter().filter_map(|&source_file| {
            if (program.has_ts_file() || !source_file.is_declaration_file.get())
                && !should_exclude_file(source_file, program, exclude_library_symbols)
            {
                Some((source_file.path().clone(), source_file))
            } else {
                None
            }
        }));
    }
    // Create DeclarationInfos for all declarations in the source files.
    let mut infos: Vec<DeclarationInfo> = Vec::new();
    #[expect(
        clippy::iter_over_hash_type,
        reason = "infos are sorted by a total key (score, name, path, pos) before truncation; Go ranges maps too"
    )]
    for source_file in source_files.values() {
        if ctx.err().is_some() {
            return Ok(lsproto::SymbolInformationsOrWorkspaceSymbolsOrNull::default());
        }
        let declaration_map = source_file.get_declaration_map();
        infos.extend(
            declaration_map
                .iter()
                .filter_map(|(name, declarations)| {
                    get_match_score(name, query).map(|score| (name, declarations, score))
                })
                .flat_map(|(name, declarations, score)| {
                    declarations.iter().map(move |&declaration| DeclarationInfo {
                        name: name.clone(),
                        declaration,
                        match_score: score,
                    })
                }),
        );
    }
    // Sort the DeclarationInfos and return the top 256 matches.
    goslices::sort_func(&mut infos, compare_declaration_infos);
    let count = infos.len().min(256);
    let mut symbols: Vec<lsproto::SymbolInformation> = Vec::with_capacity(count);
    symbols.extend(infos[0..count].iter().filter_map(|info| {
        let node = info.declaration;
        let source_file = ast::get_source_file_of_node(node).unwrap();
        let container = get_container_node(info.declaration);
        let mut container_name: Option<String> = None;
        if let Some(container) = container {
            container_name = str_ptr_to(&ast::get_declaration_name(container));
        }
        // Use the name node's span so that VS selects just the symbol name (matching
        // the TS5 navto behaviour). GetNameOfDeclaration is always non-nil here because
        // computeDeclarationMap only adds declarations whose GetDeclarationName (string
        // form) is non-empty, which implies a name node exists.
        let name_node = ast::get_name_of_declaration(node).unwrap();
        let name_start =
            astnav::get_start_of_node(name_node, source_file, false /*includeJsDoc*/);
        let name_range = TextRange::new(
            name_start,
            name_node.end().expect_source("workspace-symbol names have source positions"),
        );
        let (location, fidelity) = converters.to_lsp_location_for_feature(
            &source_file,
            name_range,
            Feature::DocumentSymbols,
        );
        if !fidelity.is_single_segment() {
            // The name has no counterpart in the original text, so there is nothing to navigate to.
            return None;
        }
        let symbol = lsproto::SymbolInformation {
            name: info.name.clone(),
            kind: get_symbol_kind_from_node(info.declaration),
            location,
            container_name,
            ..Default::default()
        };
        Some(symbol)
    }));

    Ok(lsproto::SymbolInformationsOrWorkspaceSymbolsOrNull {
        symbol_informations: Some(symbols),
        ..Default::default()
    })
}

// symbols.go:615
fn should_exclude_file(
    file: P<SourceFile>,
    program: &Program,
    exclude_library_symbols: bool,
) -> bool {
    exclude_library_symbols
        && (is_inside_node_modules(file.file_name()) || program.is_lib_file(file))
}

// symbols.go:619
pub fn is_inside_node_modules(file_name: &str) -> bool {
    file_name.contains("/node_modules/")
}

// Go `unicode.IsUpper` (category Lu). ASCII is exact; outside ASCII `char::is_uppercase` (the Uppercase property: Lu
// plus Other_Uppercase, e.g. circled letters) stands in.
fn go_is_upper(r: char) -> bool {
    if r.is_ascii() {
        return r.is_ascii_uppercase();
    }
    r.is_uppercase()
}

fn go_to_lower(r: char) -> stringutil::Rune {
    stringutil::unicode_to_lower(r as stringutil::Rune)
}

// Return a score for matching `s` against `pattern`. In order to match, `s` must contain each of the characters in
// `pattern` in the same order. Upper case characters in `pattern` must match exactly, whereas lower case characters
// in `pattern` match either case in `s`. If `s` doesn't match, -1 is returned. Otherwise, the returned score is the
// number of characters in `s` that weren't matched. Thus, zero represents an exact match, and higher values represent
// increasingly less specific partial matches.
// symbols.go:628
fn get_match_score(s: &str, pattern: &str) -> Option<usize> {
    let mut score = 0;
    let mut s = s.chars();
    for p in pattern.chars() {
        let exact = go_is_upper(p);
        loop {
            let c = s.next()?;
            if exact && c == p || !exact && go_to_lower(c) == go_to_lower(p) {
                break;
            }
            score += 1;
        }
    }
    Some(score)
}

// Sort DeclarationInfos by ascending match score, then ascending case insensitive name, then
// ascending case sensitive name, and finally by source file name and position.
// symbols.go:649
fn compare_declaration_infos(d1: &DeclarationInfo, d2: &DeclarationInfo) -> Ordering {
    if d1.match_score != d2.match_score {
        return d1.match_score.cmp(&d2.match_score);
    }
    let c = stringutil::compare_strings_case_insensitive(&d1.name, &d2.name);
    if !c.is_eq() {
        return c;
    }
    let c = d1.name.cmp(&d2.name);
    if !c.is_eq() {
        return c;
    }
    let s1 = ast::get_source_file_of_node(d1.declaration).unwrap();
    let s2 = ast::get_source_file_of_node(d2.declaration).unwrap();
    if s1 != s2 {
        return s1.path().as_str().cmp(s2.path().as_str());
    }
    d1.declaration.pos().cmp(&d2.declaration.pos())
}

// getSymbolKindFromNode converts an AST node to an LSP SymbolKind.
// Combines getNodeKind with VS Code's fromProtocolScriptElementKind.
// symbols.go:669
pub fn get_symbol_kind_from_node(node: P<Node>) -> lsproto::SymbolKind {
    match node.kind() {
        Kind::SourceFile => {
            if ast::is_external_module(node.as_source_file_p()) {
                return lsproto::SymbolKind::MODULE;
            }
            return lsproto::SymbolKind::FILE;
        }
        Kind::ModuleDeclaration => return lsproto::SymbolKind::NAMESPACE,
        Kind::ClassDeclaration | Kind::ClassExpression => return lsproto::SymbolKind::CLASS,
        Kind::InterfaceDeclaration => return lsproto::SymbolKind::INTERFACE,
        Kind::TypeAliasDeclaration | Kind::JSDocTypedefTag | Kind::JSDocCallbackTag => {
            return lsproto::SymbolKind::CLASS;
        }
        Kind::EnumDeclaration => return lsproto::SymbolKind::ENUM,
        Kind::VariableDeclaration => return lsproto::SymbolKind::VARIABLE,
        Kind::ArrowFunction | Kind::FunctionDeclaration | Kind::FunctionExpression => {
            return lsproto::SymbolKind::FUNCTION;
        }
        Kind::GetAccessor
        | Kind::SetAccessor
        | Kind::PropertyDeclaration
        | Kind::PropertySignature
        | Kind::PropertyAssignment
        | Kind::ShorthandPropertyAssignment
        | Kind::SpreadAssignment
        | Kind::IndexSignature => return lsproto::SymbolKind::PROPERTY,
        Kind::MethodDeclaration | Kind::MethodSignature | Kind::CallSignature => {
            return lsproto::SymbolKind::METHOD;
        }
        Kind::ConstructSignature => return lsproto::SymbolKind::CONSTRUCTOR,
        Kind::Constructor | Kind::ClassStaticBlockDeclaration => {
            return lsproto::SymbolKind::CONSTRUCTOR;
        }
        Kind::TypeParameter => return lsproto::SymbolKind::TYPE_PARAMETER,
        Kind::EnumMember => return lsproto::SymbolKind::ENUM_MEMBER,
        Kind::Parameter => {
            if ast::has_syntactic_modifier(node, ModifierFlags::ParameterPropertyModifier) {
                return lsproto::SymbolKind::PROPERTY;
            }
            return lsproto::SymbolKind::VARIABLE;
        }
        Kind::BinaryExpression | Kind::CallExpression => {
            let kind = ast::get_assignment_declaration_kind(node);
            if matches!(
                kind,
                JSDeclarationKind::ThisProperty
                    | JSDeclarationKind::Property
                    | JSDeclarationKind::ObjectDefinePropertyValue
            ) {
                return lsproto::SymbolKind::PROPERTY;
            }
        }
        Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral | Kind::NumericLiteral => {
            // String literals used as property names (e.g., in Object.defineProperty)
            return lsproto::SymbolKind::PROPERTY;
        }
        _ => {}
    }
    lsproto::SymbolKind::VARIABLE
}
