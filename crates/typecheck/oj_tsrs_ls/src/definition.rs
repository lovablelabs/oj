use rustc_hash::FxHashSet;
use tsrs_ast::{self as ast, Kind, Node, SourceFile, Symbol, SymbolFlags};
use tsrs_checker::{Checker, ContextFlags, Type, TypeExt as _};
use tsrs_compiler::Program;
use tsrs_core::context::Context;
use tsrs_core::{P, TextRange};
use tsrs_lsproto as lsproto;
use tsrs_scanner as scanner;

use crate::astnav;
use crate::findallreferences::{RefInfo, get_context_node};
use crate::languageservice::LanguageService;
use crate::lsconv;
use crate::spanmap::Feature;
use crate::utilities::{
    create_range_from_node, get_containing_object_literal_element, get_reference_at_position,
    get_target_label, to_context_range,
};

impl LanguageService {
    // definition.go:19
    pub fn provide_definition(
        &self,
        ctx: &Context,
        document_uri: &lsproto::DocumentUri,
        position: lsproto::Position,
    ) -> Result<lsproto::DefinitionResponse, lsproto::Error> {
        if self.user_preferences().prefer_go_to_source_definition {
            return self.provide_source_definition(ctx, document_uri, position);
        }
        self.provide_definition_worker(ctx, document_uri, position)
    }

    // definition.go:30
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    fn provide_definition_worker(
        &self,
        ctx: &Context,
        document_uri: &lsproto::DocumentUri,
        position: lsproto::Position,
    ) -> Result<lsproto::DefinitionResponse, lsproto::Error> {
        let caps = lsproto::get_client_capabilities(ctx);
        let client_supports_link = caps.text_document.definition.link_support;

        let (program, file) = self.get_program_and_file(document_uri);
        let positions =
            self.converters.from_lsp_position_for_source_file(file, position, Feature::Definition);
        let mut results: Vec<lsproto::DefinitionResponse> = Vec::with_capacity(positions.len());
        results.extend(positions.into_iter().filter_map(|mapped| {
            if mapped.fidelity.is_single_segment() {
                Some(self.provide_definition_at_position(
                    ctx,
                    program,
                    mapped.script,
                    mapped.position,
                    client_supports_link,
                ))
            } else {
                None
            }
        }));
        Ok(combine_definition_responses(results, client_supports_link))
    }

    // definition.go:49
    pub(crate) fn provide_definition_at_position(
        &self,
        ctx: &Context,
        program: &'static Program,
        file: P<SourceFile>,
        text_pos: u32,
        client_supports_link: bool,
    ) -> lsproto::DefinitionResponse {
        let pos = text_pos;
        let node = astnav::get_touching_property_name(file, pos);
        let reference = get_reference_at_position(file, pos, program);

        if node.kind() == Kind::SourceFile {
            return lsproto::LocationOrLocationsOrDefinitionLinksOrNull::default();
        }

        let (origin_selection_range, _) = self.create_lsp_range_from_node(node, file);
        if let Some(reference) = &reference
            && reference.file.is_some()
        {
            return self.create_definition_locations(
                origin_selection_range,
                client_supports_link,
                &[],
                Some(reference),
                Feature::Definition,
            );
        }

        let mut c = program.get_type_checker_for_file(ctx, file);
        let c: &mut Checker = &mut c;

        if node.kind() == Kind::OverrideKeyword
            && let Some(sym) = get_symbol_for_overridden_member(c, node)
        {
            return self.create_definition_locations(
                origin_selection_range,
                client_supports_link,
                sym.declarations(),
                None, /*reference*/
                Feature::Definition,
            );
        }

        if ast::is_jump_statement_target(node)
            && let Some(label) = get_target_label(node.parent(), node.text())
        {
            return self.create_definition_locations(
                origin_selection_range,
                client_supports_link,
                &[label],
                None, /*reference*/
                Feature::Definition,
            );
        }

        if (node.kind() == Kind::CaseKeyword
            || node.kind() == Kind::DefaultKeyword
                && ast::is_default_clause(node.parent().unwrap()))
            && let Some(stmt) = ast::find_ancestor(node.parent(), ast::is_switch_statement)
        {
            let file = ast::get_source_file_of_node(stmt).unwrap();
            return self.create_location_from_file_and_range(
                file,
                scanner::get_range_of_token_at_position(
                    file,
                    stmt.pos().expect_source("parsed switch statements have source positions"),
                ),
                Feature::Definition,
            );
        }

        if (node.kind() == Kind::ReturnKeyword
            || node.kind() == Kind::YieldKeyword
            || node.kind() == Kind::AwaitKeyword)
            && let Some(f) = ast::find_ancestor(node, ast::is_function_like_declaration)
        {
            return self.create_definition_locations(
                origin_selection_range,
                client_supports_link,
                &[f],
                None, /*reference*/
                Feature::Definition,
            );
        }

        let mut declarations = get_declarations_from_location(c, node);
        let called_declaration = try_get_signature_declaration(c, node);
        if let Some(called_declaration) = called_declaration
            && !(ast::is_jsx_opening_like_element(node.parent().unwrap())
                && is_jsx_constructor_like(called_declaration))
        {
            let symbol = c.get_symbol_at_location_exported(get_declaration_name_for_keyword(node));
            let matches = match symbol {
                Some(symbol) => c.get_root_symbols(symbol).into_iter().any(|root_symbol| {
                    symbol_matches_signature(Some(root_symbol), Some(called_declaration))
                }),
                None => false,
            };
            if matches {
                if ast::is_constructor_declaration(called_declaration) {
                    declarations.retain(|&node| {
                        node != called_declaration
                            && (ast::is_class_declaration(node) || ast::is_class_expression(node))
                    });
                } else {
                    declarations = Vec::new();
                }
            } else {
                declarations.retain(|&node| node != called_declaration);
            }
            declarations.push(called_declaration);
        }
        self.create_definition_locations(
            origin_selection_range,
            client_supports_link,
            &declarations,
            reference.as_ref(),
            Feature::Definition,
        )
    }

    // definition.go:113
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    pub fn provide_type_definition(
        &self,
        ctx: &Context,
        document_uri: &lsproto::DocumentUri,
        position: lsproto::Position,
    ) -> Result<lsproto::TypeDefinitionResponse, lsproto::Error> {
        let caps = lsproto::get_client_capabilities(ctx);
        let client_supports_link = caps.text_document.type_definition.link_support;

        let (program, file) = self.get_program_and_file(document_uri);
        let positions = self.converters.from_lsp_position_for_source_file(
            file,
            position,
            Feature::TypeDefinition,
        );
        let mut results: Vec<lsproto::TypeDefinitionResponse> = Vec::with_capacity(positions.len());
        results.extend(positions.into_iter().filter_map(|mapped| {
            if mapped.fidelity.is_single_segment() {
                Some(self.provide_type_definition_at_position(
                    ctx,
                    program,
                    mapped.script,
                    mapped.position,
                    client_supports_link,
                ))
            } else {
                None
            }
        }));
        Ok(combine_definition_responses(results, client_supports_link))
    }

    // definition.go:132
    fn provide_type_definition_at_position(
        &self,
        ctx: &Context,
        program: &'static Program,
        file: P<SourceFile>,
        text_pos: u32,
        client_supports_link: bool,
    ) -> lsproto::TypeDefinitionResponse {
        let pos = text_pos;
        let node = astnav::get_touching_property_name(file, pos);
        if node.kind() == Kind::SourceFile {
            return lsproto::LocationOrLocationsOrDefinitionLinksOrNull::default();
        }
        let (origin_selection_range, _) = self.create_lsp_range_from_node(node, file);

        let mut c = program.get_type_checker_for_file(ctx, file);
        let c: &mut Checker = &mut c;

        let node = get_declaration_name_for_keyword(node);

        if let Some(symbol) = c.get_symbol_at_location_exported(node) {
            let symbol_type = get_type_of_symbol_at_location(c, symbol, node);
            let mut declarations = get_declarations_from_type(symbol_type);
            if let Some(type_argument) = c.get_first_type_argument_from_known_type(symbol_type) {
                let mut d = get_declarations_from_type(type_argument);
                d.extend(declarations);
                declarations = d;
            }
            if !declarations.is_empty() {
                return self.create_definition_locations(
                    origin_selection_range,
                    client_supports_link,
                    &declarations,
                    None, /*reference*/
                    Feature::TypeDefinition,
                );
            }
            if !symbol.flags().intersects(SymbolFlags::Value)
                && symbol.flags().intersects(SymbolFlags::Type)
            {
                return self.create_definition_locations(
                    origin_selection_range,
                    client_supports_link,
                    symbol.declarations(),
                    None, /*reference*/
                    Feature::TypeDefinition,
                );
            }
        }

        lsproto::LocationOrLocationsOrDefinitionLinksOrNull::default()
    }
}

// definition.go:162
pub fn combine_definition_responses(
    results: Vec<lsproto::DefinitionResponse>,
    links: bool,
) -> lsproto::DefinitionResponse {
    let mut locations: Vec<lsproto::Location> = Vec::new();
    let mut definition_links: Vec<lsproto::LocationLink> = Vec::new();
    let mut seen: FxHashSet<lsproto::Location> = FxHashSet::default();
    for result in results {
        if let Some(result_links) = result.definition_links {
            (definition_links, locations) = result_links.into_iter().fold(
                (definition_links, locations),
                |(mut definition_links, mut locations), link| {
                    let location = lsproto::Location {
                        uri: link.target_uri.clone(),
                        range: link.target_selection_range,
                    };
                    if seen.insert(location.clone()) {
                        definition_links.push(link);
                        locations.push(location);
                    }

                    (definition_links, locations)
                },
            );
        }
        if let Some(location) = result.location
            && seen.insert(location.clone())
        {
            definition_links.push(lsproto::LocationLink {
                target_uri: location.uri.clone(),
                target_range: location.range,
                target_selection_range: location.range,
                ..Default::default()
            });
            locations.push(location);
        }
        if let Some(result_locations) = result.locations {
            (definition_links, locations) = result_locations.into_iter().fold(
                (definition_links, locations),
                |(mut definition_links, mut locations), location| {
                    if seen.insert(location.clone()) {
                        definition_links.push(lsproto::LocationLink {
                            target_uri: location.uri.clone(),
                            target_range: location.range,
                            target_selection_range: location.range,
                            ..Default::default()
                        });
                        locations.push(location);
                    }

                    (definition_links, locations)
                },
            );
        }
    }
    if links {
        return lsproto::LocationOrLocationsOrDefinitionLinksOrNull {
            definition_links: Some(definition_links),
            ..Default::default()
        };
    }
    lsproto::LocationOrLocationsOrDefinitionLinksOrNull {
        locations: Some(locations),
        ..Default::default()
    }
}

// definition.go:195
pub fn get_declaration_name_for_keyword(node: P<Node>) -> P<Node> {
    if node.kind() >= Kind::FIRST_KEYWORD && node.kind() <= Kind::LAST_KEYWORD {
        let parent = node.parent().unwrap();
        if ast::is_variable_declaration_list(parent) {
            if let Some(&decl) = parent.as_variable_declaration_list().declarations.nodes().first()
                && let Some(name) = decl.name()
            {
                return name;
            }
        } else if parent.declaration_data().is_some()
            && let Some(name) = parent.name()
            && node.pos() < name.pos()
        {
            return name;
        }
    }
    node
}

// definition.go:208
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FileRange {
    file: P<SourceFile>,
    file_range: TextRange,
}

impl LanguageService {
    // definition.go:213
    pub(crate) fn create_definition_locations(
        &self,
        origin_selection_range: lsproto::Range,
        client_supports_link: bool,
        declarations: &[P<Node>],
        reference: Option<&RefInfo>,
        feature: Feature,
    ) -> lsproto::DefinitionResponse {
        let mut locations: Vec<lsproto::LocationLink> = Vec::new();
        let mut location_ranges: FxHashSet<FileRange> = FxHashSet::default();

        if let Some(reference) = reference {
            let target_range = lsproto::Range {
                start: lsproto::Position { line: 0, character: 0 },
                end: lsproto::Position { line: 0, character: 0 },
            };
            locations.push(lsproto::LocationLink {
                origin_selection_range: Some(origin_selection_range),
                target_uri: lsconv::file_name_to_document_uri(&reference.file_name),
                target_range,
                target_selection_range: target_range,
            });
        }

        locations.extend(declarations.iter().filter_map(|&decl| {
            let file = ast::get_source_file_of_node(decl).unwrap();
            // The `this` parameter a JSDoc `@this` tag adds has a name without a position; Go converts its -1
            // range to the start of the file. Point at the declaration (the tag name) instead.
            let name = ast::get_name_of_declaration(decl)
                .filter(|name| !name.pos().is_synthetic())
                .unwrap_or(decl);
            let name_range = if name.kind() == Kind::EmptyStatement {
                let pos = name.pos().expect_source("parsed empty statements have source positions");
                TextRange::new(pos, pos)
            } else {
                create_range_from_node(name, file)
            };
            if location_ranges.insert(FileRange { file, file_range: name_range }) {
                let context_node = get_context_node(Some(decl)).unwrap_or(decl);
                let mut context_range =
                    to_context_range(Some(name_range), file, Some(context_node))
                        .unwrap_or(name_range);
                if !name_range.contained_by(context_range) {
                    context_range = TextRange::from_text_pos(
                        name_range.pos().min(context_range.pos()),
                        name_range.end().max(context_range.end()),
                    );
                }
                let (target_selection_loc, selection_fidelity) =
                    self.source_file_range_to_lsp_location_for_feature(file, name_range, feature);
                if !selection_fidelity.is_single_segment() {
                    return None;
                }
                let (mut target_loc, context_fidelity) =
                    self.source_file_range_to_lsp_location(file, context_range);
                if context_fidelity.is_none()
                    || target_loc.uri != target_selection_loc.uri
                    || !lsp_range_contains(target_loc.range, target_selection_loc.range)
                {
                    target_loc = target_selection_loc.clone();
                }
                Some(lsproto::LocationLink {
                    origin_selection_range: Some(origin_selection_range),
                    target_selection_range: target_selection_loc.range,
                    target_uri: target_loc.uri,
                    target_range: target_loc.range,
                })
            } else {
                None
            }
        }));

        if client_supports_link {
            return lsproto::LocationOrLocationsOrDefinitionLinksOrNull {
                definition_links: Some(locations),
                ..Default::default()
            };
        }
        create_locations_from_links(locations)
    }
}

// definition.go:281
pub fn lsp_range_contains(outer: lsproto::Range, inner: lsproto::Range) -> bool {
    lsproto::compare_positions(outer.start, inner.start).is_le()
        && lsproto::compare_positions(inner.end, outer.end).is_le()
}

// definition.go:286
fn create_locations_from_links(links: Vec<lsproto::LocationLink>) -> lsproto::DefinitionResponse {
    let locations: Vec<lsproto::Location> = links
        .into_iter()
        .map(|link| lsproto::Location { uri: link.target_uri, range: link.target_selection_range })
        .collect();
    lsproto::LocationOrLocationsOrDefinitionLinksOrNull {
        locations: Some(locations),
        ..Default::default()
    }
}

impl LanguageService {
    // definition.go:296
    fn create_location_from_file_and_range(
        &self,
        file: P<SourceFile>,
        text_range: TextRange,
        feature: Feature,
    ) -> lsproto::DefinitionResponse {
        let (mut mapped_location, fidelity) =
            self.source_file_range_to_lsp_location_for_feature(file, text_range, feature);
        if fidelity.is_none() {
            mapped_location.range = lsproto::Range::default();
        }
        lsproto::LocationOrLocationsOrDefinitionLinksOrNull {
            location: Some(mapped_location),
            ..Default::default()
        }
    }
}

// definition.go:306
pub fn get_declarations_from_location(c: &mut Checker, node: P<Node>) -> Vec<P<Node>> {
    if ast::is_identifier(node) && ast::is_shorthand_property_assignment(node.parent().unwrap()) {
        // Because name in short-hand property assignment has two different meanings: property name and property value,
        // using go-to-definition at such position should go to the variable declaration of the property value rather than
        // go to the declaration of the property name (in this case stay at the same position). However, if go-to-definition
        // is performed at the location of property access, we would like to go to definition of the property in the short-hand
        // assignment. This case and others are handled by the following code.
        // and the contextual type's property declarations
        let shorthand_symbol = c.get_resolved_symbol_exported(node);
        let mut declarations: Vec<P<Node>> = shorthand_symbol.declarations().to_vec();
        let contextual_declarations = get_declarations_from_object_literal_element(c, node);
        declarations.extend(contextual_declarations);
        return declarations;
    }

    if ast::is_property_name(node)
        && ast::is_binding_element(node.parent().unwrap())
        && ast::is_object_binding_pattern(node.parent().unwrap().parent().unwrap())
    {
        // If the node is the name of a BindingElement within an ObjectBindingPattern instead of just returning the
        // declaration of the symbol (which is itself), we should try to get to the original type of the
        // ObjectBindingPattern and return the property declaration for the referenced property.
        // For example:
        //      import('./foo').then(({ bar }) => undefined); => should navigate to the declaration in file "./foo"
        //
        //      function bar<T>(onfulfilled: (value: T) => void) { }
        //      interface Test { prop1: number }
        //      bar<Test>(({ prop1 }) => {});  => should navigate to prop1 in Test
        let parent = node.parent().unwrap();
        let binding_el = parent.as_binding_element();
        if binding_el.dot_dot_dot_token().is_none()
            && Some(node) == binding_el.property_name().or(parent.name())
            && let Some(name) = ast::try_get_text_of_property_name(node)
        {
            let t = c.get_type_at_location(parent.parent().unwrap());
            let types: Vec<P<Type>> = if t.is_union() { t.types().to_vec() } else { vec![t] };
            let mut result: Vec<P<Node>> = Vec::new();
            result.extend(
                types
                    .into_iter()
                    .filter_map(|union_type| {
                        c.get_property_of_type_exported(union_type, &name)
                            .map(|prop| prop.declarations().iter().copied())
                    })
                    .flatten(),
            );
            return result;
        }
    }

    let node = get_declaration_name_for_keyword(node);
    if let Some(mut symbol) = c.get_symbol_at_location_exported(node) {
        if symbol.flags().intersects(SymbolFlags::Class)
            && !symbol.flags().intersects(SymbolFlags::Function | SymbolFlags::Variable)
            && node.kind() == Kind::ConstructorKeyword
            && let Some(constructor) =
                symbol.members().and_then(|m| m.lookup(ast::INTERNAL_SYMBOL_NAME_CONSTRUCTOR))
        {
            symbol = constructor;
        }
        if symbol.flags().intersects(SymbolFlags::Alias)
            && let (Some(resolved), true) = c.resolve_alias_exported(Some(symbol))
        {
            symbol = resolved;
        }
        let object_literal_element_declarations =
            get_declarations_from_object_literal_element(c, node);
        if !object_literal_element_declarations.is_empty() {
            return object_literal_element_declarations;
        }
        if !symbol.declarations().is_empty() {
            return symbol.declarations().to_vec();
        }
    }
    let index_infos = c.get_index_signatures_at_location_exported(node);
    if !index_infos.is_empty() {
        return index_infos;
    }
    Vec::new()
}

// getDeclarationsFromObjectLiteralElement returns declarations from the contextual type
// of an object literal element, if available.
// definition.go:380
fn get_declarations_from_object_literal_element(c: &mut Checker, node: P<Node>) -> Vec<P<Node>> {
    let Some(element) = get_containing_object_literal_element(node) else {
        return Vec::new();
    };

    let Some(contextual_type) =
        c.get_contextual_type_exported(element.parent().unwrap(), ContextFlags::None)
    else {
        return Vec::new();
    };

    let mut properties = c.get_property_symbols_from_contextual_type(
        element,
        contextual_type,
        false, /*unionSymbolOk*/
    );
    if properties.iter().any(|p| {
        p.value_declaration().is_some_and(|vd| {
            ast::is_object_literal_expression(vd.parent().unwrap())
                && ast::is_object_literal_element(vd)
                && vd.name() == Some(node)
        })
    }) && let Some(without_node_inferences_type) = c
        .get_contextual_type_exported(element.parent().unwrap(), ContextFlags::IgnoreNodeInferences)
    {
        let without_node_inferences_properties = c.get_property_symbols_from_contextual_type(
            element,
            without_node_inferences_type,
            false, /*unionSymbolOk*/
        );
        if !without_node_inferences_properties.is_empty() {
            properties = without_node_inferences_properties;
        }
    }

    let mut result: Vec<P<Node>> = Vec::new();
    result.extend(properties.into_iter().flat_map(|prop| prop.declarations().iter().copied()));
    result
}

// Returns a CallLikeExpression where `node` is the target being invoked.
// definition.go:410
fn get_ancestor_call_like_expression(node: P<Node>) -> Option<P<Node>> {
    let target = ast::find_ancestor(node, |n| !ast::is_right_side_of_property_access(n)).unwrap();
    let call_like = target.parent()?;
    if ast::is_call_like_expression(call_like) && ast::get_invoked_expression(call_like) == target {
        return Some(call_like);
    }
    None
}

// definition.go:421
pub fn try_get_signature_declaration(type_checker: &mut Checker, node: P<Node>) -> Option<P<Node>> {
    let call_like = get_ancestor_call_like_expression(node);
    let signature =
        call_like.map(|call_like| type_checker.get_resolved_signature_exported(call_like));
    // Don't go to a function type, go to the value having that type.
    if let Some(signature) = signature
        && let Some(declaration) = signature.declaration()
        && ast::is_function_like(Some(declaration))
        && !ast::is_function_type_node(declaration)
    {
        return Some(declaration);
    }
    None
}

// definition.go:438
fn is_jsx_constructor_like(node: P<Node>) -> bool {
    ast::is_constructor_declaration(node)
        || ast::is_constructor_type_node(node)
        || ast::is_call_signature_declaration(node)
        || ast::is_construct_signature_declaration(node)
}

// definition.go:450
fn symbol_matches_signature(
    symbol: Option<P<Symbol>>,
    called_declaration: Option<P<Node>>,
) -> bool {
    let (Some(symbol), Some(called_declaration)) = (symbol, called_declaration) else {
        return false;
    };
    let called_symbol = called_declaration.symbol();
    if Some(symbol) == called_symbol || called_symbol.is_some_and(|cs| Some(symbol) == cs.parent())
    {
        return true;
    }
    let Some(parent) = called_declaration.parent() else {
        return false;
    };
    ast::is_assignment_expression(parent, false /*excludeCompoundAssignment*/)
        || !ast::is_call_like_expression(parent)
            && ast::can_have_symbol(parent)
            && Some(symbol) == parent.symbol()
}

// definition.go:463
fn get_symbol_for_overridden_member(
    type_checker: &mut Checker,
    node: P<Node>,
) -> Option<P<Symbol>> {
    let class_element = ast::find_ancestor(node, ast::is_class_element)?;
    let class_element_name = class_element.name()?;
    let base_declaration = ast::find_ancestor(class_element, ast::is_class_like)?;
    let base_type_node = ast::get_class_extends_heritage_element(base_declaration)?;
    let expression = ast::skip_parentheses(base_type_node.expression().unwrap());
    let base = if ast::is_class_expression(expression) {
        expression.symbol()
    } else {
        type_checker.get_symbol_at_location_exported(expression)
    };
    let base = base?;
    let name = ast::get_text_of_property_name(class_element_name);
    if ast::has_static_modifier(class_element) {
        let t = type_checker.get_type_of_symbol_exported(base);
        return type_checker.get_property_of_type_exported(t, &name);
    }
    let t = type_checker.get_declared_type_of_symbol_exported(base);
    type_checker.get_property_of_type_exported(t, &name)
}

// definition.go:493
fn get_type_of_symbol_at_location(c: &mut Checker, symbol: P<Symbol>, node: P<Node>) -> P<Type> {
    let t = c.get_type_of_symbol_at_location(symbol, Some(node)).unwrap();
    // If the type is just a function's inferred type, go-to-type should go to the return type instead since
    // go-to-definition takes you to the function anyway.
    if t.symbol() == Some(symbol)
        || t.symbol().is_some()
            && symbol.value_declaration().is_some_and(|vd| {
                ast::is_variable_declaration(vd)
                    && vd.initializer() == t.symbol().unwrap().value_declaration()
            })
    {
        let sigs = c.get_call_signatures(t);
        if sigs.len() == 1 {
            return c.get_return_type_of_signature_exported(sigs[0]);
        }
    }
    t
}

// definition.go:506
fn get_declarations_from_type(t: P<Type>) -> Vec<P<Node>> {
    let mut result: Vec<P<Node>> = Vec::new();
    for t in t.distributed() {
        if let Some(symbol) = t.symbol() {
            for &decl in symbol.declarations() {
                if !result.contains(&decl) {
                    result.push(decl);
                }
            }
        }
    }
    result
}
