use rustc_hash::FxHashSet;
use tsrs_ast::{self as ast, Kind, Node, NodeFlags, SourceFile, Symbol};
use tsrs_checker::Checker;
use tsrs_core::{P, TextRange};
use tsrs_lsproto as lsproto;
use tsrs_scanner as scanner;

use crate::hover::get_documentation_from_declaration;
use crate::spanmap::Fidelity;

// JSDocTagInfo mirrors Strada's `JSDocTagInfo`, but renders the tag's text as a
// plain string instead of `SymbolDisplayPart[]`.
// jsdoc.go:18
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JSDocTagInfo {
    pub name: String,
    pub text: String,
}

// GetSymbolDocumentationComment renders a symbol's documentation comment as plain text.
// It backs the API's Symbol.getDocumentationComment and mirrors Strada's
// getJsDocCommentsFromDeclarations: comments are gathered from each unique declaration,
// deduplicated, and joined with line breaks. Like Strada, it does not resolve aliases —
// consumers resolve aliases themselves (via getAliasedSymbol) and re-query if desired.
// jsdoc.go:28
pub fn get_symbol_documentation_comment(c: &mut Checker, symbol: Option<P<Symbol>>) -> String {
    let Some(symbol) = symbol else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    let mut seen: FxHashSet<P<Node>> = FxHashSet::default();
    for &decl in symbol.declarations() {
        if !seen.insert(decl) {
            continue;
        }
        let doc = get_documentation_from_declaration(
            &no_mapped_location,
            c,
            Some(symbol),
            Some(decl),
            Some(decl),
            lsproto::MarkupKind::PLAIN_TEXT,
            true, /*commentOnly*/
        );
        if !doc.is_empty() && !parts.contains(&doc) {
            parts.push(doc);
        }
    }
    parts.join("\n")
}

// GetSymbolJSDocTags collects a symbol's JSDoc tags. It backs the API's Symbol.getJsDocTags
// and mirrors Strada's getJsDocTagsFromDeclarations, except each tag's text is rendered as a
// plain string rather than SymbolDisplayPart[]. Tags with no text have an empty Text field.
// jsdoc.go:52
pub fn get_symbol_jsdoc_tags(symbol: Option<P<Symbol>>) -> Vec<JSDocTagInfo> {
    let Some(symbol) = symbol else {
        return Vec::new();
    };
    let mut infos: Vec<JSDocTagInfo> = Vec::new();
    let mut seen: FxHashSet<P<Node>> = FxHashSet::default();
    for &decl in symbol.declarations() {
        if !seen.insert(decl) {
            continue;
        }
        let tags = declaration_jsdoc_tags(decl);
        // Skip comments containing @typedef/@callback since they're not associated with a
        // particular declaration, unless they also carry @param/@return (treated as local docs).
        let has_typedef = tags
            .iter()
            .any(|t| t.kind() == Kind::JSDocTypedefTag || t.kind() == Kind::JSDocCallbackTag);
        let has_param_or_return = tags
            .iter()
            .any(|t| t.kind() == Kind::JSDocParameterTag || t.kind() == Kind::JSDocReturnTag);
        if has_typedef && !has_param_or_return {
            continue;
        }
        infos.extend(tags.iter().map(|&tag| JSDocTagInfo {
            name: tag.tag_name().text().to_string(),
            text: get_jsdoc_tag_text(tag),
        }));
    }
    infos
}

// declarationJSDocTags returns the JSDoc tags associated with a declaration, walking the
// JSDoc comment location chain like the checker's getAllJSDocTags.
// jsdoc.go:85
fn declaration_jsdoc_tags(node: P<Node>) -> &'static [P<Node>] {
    if !node.flags().intersects(NodeFlags::JSDoc) {
        let mut current = Some(node);
        while let Some(cur) = current {
            let jsdocs = cur.jsdoc(None);
            if !jsdocs.is_empty() {
                let last_jsdoc = jsdocs[jsdocs.len() - 1].as_jsdoc();
                if let Some(tags) = last_jsdoc.tags {
                    return tags.nodes();
                }
            }
            current = ast::get_next_jsdoc_comment_location(cur);
        }
    }
    &[]
}

// getJSDocTagText renders the text of a single JSDoc tag as a plain string, mirroring
// Strada's getCommentDisplayParts collapsed from SymbolDisplayPart[] to a string.
// jsdoc.go:103
fn get_jsdoc_tag_text(tag: P<Node>) -> String {
    let comment = scanner::get_text_of_jsdoc_comment(tag.comment_list());
    let add_comment = |s: String| -> String {
        if comment.is_empty() {
            return s;
        }
        format!("{s} {comment}")
    };
    match tag.kind() {
        Kind::JSDocThrowsTag => {
            if let Some(te) = tag.as_jsdoc_throws_tag().type_expression {
                return add_comment(scanner::get_text_of_node(te));
            }
            comment
        }
        Kind::JSDocImplementsTag => {
            add_comment(scanner::get_text_of_node(tag.as_jsdoc_implements_tag().class_name))
        }
        Kind::JSDocAugmentsTag => {
            add_comment(scanner::get_text_of_node(tag.as_jsdoc_augments_tag().class_name))
        }
        Kind::JSDocTemplateTag => {
            let template_tag = tag.as_jsdoc_template_tag();
            let mut b = String::new();
            if let Some(constraint) = template_tag.constraint {
                b.push_str(&scanner::get_text_of_node(constraint));
            }
            b = template_tag.type_parameters.nodes().iter().enumerate().fold(
                b,
                |mut b, (i, &tp)| {
                    if i == 0 && !b.is_empty() {
                        b.push(' ');
                    }
                    if i != 0 {
                        b.push_str(", ");
                    }
                    b.push_str(&scanner::get_text_of_node(tp));

                    b
                },
            );
            if !comment.is_empty() {
                if !b.is_empty() {
                    b.push(' ');
                }
                b.push_str(&comment);
            }
            b
        }
        Kind::JSDocTypeTag => {
            add_comment(scanner::get_text_of_node(tag.as_jsdoc_type_tag().type_expression))
        }
        Kind::JSDocSatisfiesTag => {
            add_comment(scanner::get_text_of_node(tag.as_jsdoc_satisfies_tag().type_expression))
        }
        Kind::JSDocSeeTag => {
            if let Some(ne) = tag.as_jsdoc_see_tag().name_expression {
                return add_comment(scanner::get_text_of_node(ne));
            }
            comment
        }
        Kind::JSDocParameterTag | Kind::JSDocPropertyTag => {
            if let Some(name) = tag.name() {
                return add_comment(scanner::get_text_of_node(name));
            }
            comment
        }
        _ => comment,
    }
}

// jsdoc.go:162
pub fn get_jsdoc(node: P<Node>) -> Option<P<Node>> {
    node.jsdoc(None).last().copied()
}

// jsdoc.go:166
pub fn get_jsdoc_or_tag(
    c: &mut Checker,
    node: Option<P<Node>>,
    seen_symbols: &mut FxHashSet<P<Symbol>>,
) -> Option<P<Node>> {
    let node = node?;
    if let Some(jsdoc) = get_jsdoc(node) {
        return Some(jsdoc);
    }
    if ast::is_parameter_declaration(node) {
        let name = node.name().unwrap();
        if ast::is_binding_pattern(name) {
            // For binding patterns, match JSDoc @param tags by position rather than by name
            return get_jsdoc_parameter_tag_by_position(c, node);
        }
        return get_matching_jsdoc_tag(
            c,
            node.parent().unwrap(),
            name.text(),
            is_matching_parameter_tag,
            seen_symbols,
        );
    } else if ast::is_type_parameter_declaration(node) {
        return get_matching_jsdoc_tag(
            c,
            node.parent().unwrap(),
            node.name().unwrap().text(),
            is_matching_template_tag,
            seen_symbols,
        );
    } else if ast::is_variable_declaration(node)
        && ast::is_variable_declaration_list(node.parent().unwrap())
        && node.parent().unwrap().as_variable_declaration_list().declarations.nodes().first()
            == Some(&node)
    {
        return get_jsdoc_or_tag(c, node.parent().unwrap().parent(), seen_symbols);
    } else if (ast::is_function_expression_or_arrow_function(node)
        || ast::is_class_expression(node))
        && (ast::is_variable_declaration(node.parent().unwrap())
            || ast::is_property_declaration(node.parent().unwrap())
            || ast::is_property_assignment(node.parent().unwrap()))
        && node.parent().unwrap().initializer() == Some(node)
    {
        return get_jsdoc_or_tag(c, node.parent(), seen_symbols);
    } else if ast::is_binding_element(node)
        && ast::is_object_binding_pattern(node.parent().unwrap())
    {
        let name = node.property_name_or_name().unwrap();
        if ast::is_identifier(name) {
            let object_type = c.get_type_at_location(node.parent().unwrap());
            if let Some(prop) = c.get_property_of_type_exported(object_type, name.text())
                && let Some(result) = prop.declarations().iter().find_map(|&d| {
                    if let Some(jsdoc) = get_jsdoc(d) {
                        return Some(Some(jsdoc));
                    }

                    None
                })
            {
                return result;
            }
        }
    }
    if let Some(symbol) = node.symbol()
        && let Some(parent) = node.parent()
    {
        if ast::is_function_declaration(node)
            || ast::is_method_declaration(node)
            || ast::is_method_signature_declaration(node)
            || ast::is_constructor_declaration(node)
            || ast::is_construct_signature_declaration(node)
        {
            let first_signature =
                symbol.declarations().iter().copied().find(|&d| ast::is_function_like(Some(d)));
            if let Some(first_signature) = first_signature
                && node != first_signature
                && let Some(js_doc) = get_jsdoc_or_tag(c, Some(first_signature), seen_symbols)
            {
                return Some(js_doc);
            }
        }
        if ast::is_class_or_interface_like(parent) {
            let is_static = ast::has_static_modifier(node);
            let class_type = c.get_declared_type_of_symbol_exported(parent.symbol().unwrap());
            if is_static {
                // For static members, use the checker's base constructor type resolution.
                // This correctly handles intersection constructor types from mixins
                // (e.g., typeof MixinClass & T) by preserving the full intersection.
                let base_constructor_type =
                    c.get_base_constructor_type_of_class_exported(class_type);
                let static_base_type = c.get_apparent_type_exported(base_constructor_type);
                if let Some(prop) = c.get_property_of_type_exported(static_base_type, symbol.name())
                    && let Some(value_declaration) = prop.value_declaration()
                    && seen_symbols.insert(prop)
                    && let Some(js_doc) = get_jsdoc_or_tag(c, Some(value_declaration), seen_symbols)
                {
                    return Some(js_doc);
                }
            } else if let Some(result) =
                c.get_base_types_exported(class_type).iter().find_map(|&base_type| {
                    if let Some(prop) = c.get_property_of_type_exported(base_type, symbol.name())
                        && let Some(value_declaration) = prop.value_declaration()
                        && seen_symbols.insert(prop)
                        && let Some(js_doc) =
                            get_jsdoc_or_tag(c, Some(value_declaration), seen_symbols)
                    {
                        return Some(Some(js_doc));
                    }

                    None
                })
            {
                return result;
            }
        }
    }
    None
}

// jsdoc.go:230
fn get_matching_jsdoc_tag(
    c: &mut Checker,
    node: P<Node>,
    name: &str,
    match_: fn(P<Node>, &str) -> bool,
    seen_symbols: &mut FxHashSet<P<Symbol>>,
) -> Option<P<Node>> {
    if let Some(jsdoc) = get_jsdoc_or_tag(c, Some(node), seen_symbols)
        && jsdoc.kind() == Kind::JSDoc
        && let Some(tags) = jsdoc.as_jsdoc().tags
        && let Some(result) = tags
            .nodes()
            .iter()
            .find_map(|&tag| if match_(tag, name) { Some(Some(tag)) } else { None })
    {
        return result;
    }
    None
}

// getJSDocParameterTagByPosition finds a JSDoc @param tag for a binding pattern parameter by position.
// Since binding patterns don't have a simple name, we match the @param tag at the same index as the parameter.
// jsdoc.go:245
fn get_jsdoc_parameter_tag_by_position(c: &mut Checker, param: P<Node>) -> Option<P<Node>> {
    let parent = param.parent()?;

    // Find the parameter's index in the parent's parameters list
    let params = parent.parameters();
    let param_index = params.iter().position(|&p| p == param)?;

    // Get the JSDoc for the parent function/method
    let jsdoc = get_jsdoc_or_tag(c, Some(parent), &mut FxHashSet::default())?;
    if jsdoc.kind() != Kind::JSDoc {
        return None;
    }

    // Collect all @param tags in order
    let tags = jsdoc.as_jsdoc().tags?;

    if let Some(&tag) =
        tags.nodes().iter().filter(|tag| tag.kind() == Kind::JSDocParameterTag).nth(param_index)
    {
        return Some(tag);
    }
    None
}

// jsdoc.go:291
fn is_matching_parameter_tag(tag: P<Node>, name: &str) -> bool {
    tag.kind() == Kind::JSDocParameterTag && is_node_with_name(tag, name)
}

// jsdoc.go:295
fn is_matching_template_tag(tag: P<Node>, name: &str) -> bool {
    tag.kind() == Kind::JSDocTemplateTag
        && tag.type_parameters().iter().any(|&tp| is_node_with_name(tp, name))
}

// jsdoc.go:299
fn is_node_with_name(node: P<Node>, name: &str) -> bool {
    match node.name() {
        Some(node_name) => ast::is_identifier(node_name) && node_name.text() == name,
        None => false,
    }
}

// jsdoc.go:304
pub fn no_mapped_location(
    _file: P<SourceFile>,
    _range: TextRange,
) -> (lsproto::Location, Fidelity) {
    (lsproto::Location::default(), Fidelity::None)
}
