use tsrs_ast::{self as ast, Kind, Node, SourceFile};
use tsrs_core::collections::OrderedMap;
use tsrs_core::stringutil;
use tsrs_core::{P, TextRange};
use tsrs_scanner as scanner;

use super::tracker::*;
use crate::astnav;
use crate::format;

// delete.go:17
// deleteDeclaration deletes a node with smart handling for different node types.
// This handles special cases like import specifiers in lists, parameters, etc.
pub fn delete_declaration(
    t: &mut Tracker,
    deleted_nodes_in_lists: &mut OrderedMap<P<Node>, bool>,
    source_file: P<SourceFile>,
    node: P<Node>,
) {
    match node.kind() {
        Kind::Parameter => {
            let old_function = node.parent().unwrap();
            if old_function.kind() == Kind::ArrowFunction
                && old_function.parameters().len() == 1
                && astnav::find_child_of_kind(old_function, Kind::OpenParenToken, source_file)
                    .is_none()
            {
                // Lambdas with exactly one parameter are special because, after removal, there
                // must be an empty parameter list (i.e. `()`) and this won't necessarily be the
                // case if the parameter is simply removed (e.g. in `x => 1`).
                let range = t.get_adjusted_range(
                    source_file,
                    node,
                    node,
                    LeadingTriviaOption::IncludeAll,
                    TrailingTriviaOption::Include,
                );
                t.replace_text_range_with_text(source_file, range, "()");
            } else {
                delete_node_in_list(t, deleted_nodes_in_lists, source_file, node);
            }
        }

        Kind::ImportDeclaration | Kind::ImportEqualsDeclaration => {
            let imports = source_file.imports();
            let is_first_import = !imports.is_empty() && Some(node) == imports[0].parent()
                || Some(node)
                    == source_file
                        .statements
                        .nodes()
                        .iter()
                        .copied()
                        .find(|&s| ast::is_any_import_syntax(s));
            // For first import, leave header comment in place, otherwise only delete JSDoc comments
            let mut leading_trivia = LeadingTriviaOption::StartLine;
            if is_first_import {
                leading_trivia = LeadingTriviaOption::Exclude;
            } else if has_jsdoc_nodes(Some(node)) {
                leading_trivia = LeadingTriviaOption::JSDoc;
            }
            delete_node(t, source_file, node, leading_trivia, TrailingTriviaOption::Include);
        }

        Kind::BindingElement => {
            let pattern = node.parent().unwrap();
            let elements = pattern.as_binding_pattern().elements.nodes();
            let preserve_comma =
                pattern.kind() == Kind::ArrayBindingPattern && node != elements[elements.len() - 1];
            if preserve_comma {
                delete_node(
                    t,
                    source_file,
                    node,
                    LeadingTriviaOption::IncludeAll,
                    TrailingTriviaOption::Exclude,
                );
            } else {
                delete_node_in_list(t, deleted_nodes_in_lists, source_file, node);
            }
        }

        Kind::VariableDeclaration => {
            delete_variable_declaration(t, deleted_nodes_in_lists, source_file, node);
        }

        Kind::TypeParameter => delete_node_in_list(t, deleted_nodes_in_lists, source_file, node),

        Kind::ImportSpecifier => {
            let named_imports = node.parent().unwrap();
            if named_imports.as_named_imports().elements.nodes().len() == 1 {
                delete_import_binding(t, source_file, named_imports);
            } else {
                delete_node_in_list(t, deleted_nodes_in_lists, source_file, node);
            }
        }

        Kind::NamespaceImport => delete_import_binding(t, source_file, node),

        Kind::SemicolonToken => delete_node(
            t,
            source_file,
            node,
            LeadingTriviaOption::IncludeAll,
            TrailingTriviaOption::Exclude,
        ),

        Kind::TypeKeyword => {
            // For type keyword in import clauses, we need to delete the keyword and any trailing space
            // The trailing space is part of the next token's leading trivia, so we include it
            delete_node(
                t,
                source_file,
                node,
                LeadingTriviaOption::Exclude,
                TrailingTriviaOption::Include,
            );
        }

        Kind::FunctionKeyword => delete_node(
            t,
            source_file,
            node,
            LeadingTriviaOption::Exclude,
            TrailingTriviaOption::Include,
        ),

        Kind::ClassDeclaration | Kind::FunctionDeclaration => {
            let mut leading_trivia = LeadingTriviaOption::StartLine;
            if has_jsdoc_nodes(Some(node)) {
                leading_trivia = LeadingTriviaOption::JSDoc;
            }
            delete_node(t, source_file, node, leading_trivia, TrailingTriviaOption::Include);
        }

        _ => {
            match node.parent() {
                None => {
                    // a misbehaving client can reach here with the SourceFile node
                    delete_node(
                        t,
                        source_file,
                        node,
                        LeadingTriviaOption::IncludeAll,
                        TrailingTriviaOption::Include,
                    );
                }
                Some(parent)
                    if parent.kind() == Kind::ImportClause
                        && parent.as_import_clause().name() == Some(node) =>
                {
                    delete_default_import(t, source_file, parent);
                }
                Some(parent)
                    if parent.kind() == Kind::CallExpression
                        && parent.arguments().contains(&node) =>
                {
                    delete_node_in_list(t, deleted_nodes_in_lists, source_file, node);
                }
                Some(_) => delete_node(
                    t,
                    source_file,
                    node,
                    LeadingTriviaOption::IncludeAll,
                    TrailingTriviaOption::Include,
                ),
            }
        }
    }
}

// delete.go:105
fn delete_default_import(t: &mut Tracker, source_file: P<SourceFile>, import_clause: P<Node>) {
    let clause = import_clause.as_import_clause();
    if clause.named_bindings.is_none() {
        // Delete the whole import
        delete_node(
            t,
            source_file,
            import_clause.parent().unwrap(),
            LeadingTriviaOption::IncludeAll,
            TrailingTriviaOption::Include,
        );
    } else {
        // import |d,| * as ns from './file'
        let name = clause.name().unwrap();
        let start = astnav::get_start_of_node(name, source_file, false);
        let next_token = astnav::get_token_at_position(
            source_file,
            name.end().expect_source("parsed import names have source positions"),
        );
        if next_token.kind() == Kind::CommaToken {
            // shift first non-whitespace position after comma to the start position of the node
            let end = scanner::skip_trivia_ex(
                source_file.text(),
                next_token.end().expect_source("parsed tokens have source positions"),
                Some(&scanner::SkipTriviaOptions {
                    stop_after_line_break: false,
                    stop_at_comments: true,
                    ..Default::default()
                }),
            );
            t.replace_text_range_with_text(source_file, TextRange::new(start, end), "");
        } else {
            delete_node(
                t,
                source_file,
                name,
                LeadingTriviaOption::IncludeAll,
                TrailingTriviaOption::Include,
            );
        }
    }
}

// delete.go:126
fn delete_import_binding(t: &mut Tracker, source_file: P<SourceFile>, node: P<Node>) {
    let import_clause = node.parent().unwrap().as_import_clause();
    if import_clause.name().is_some() {
        // Delete named imports while preserving the default import
        // import d|, * as ns| from './file'
        // import d|, { a }| from './file'
        let previous_token = astnav::get_token_at_position(
            source_file,
            node.pos().expect_source("parsed import bindings have source positions") - 1,
        );
        let start = astnav::get_start_of_node(previous_token, source_file, false);
        t.replace_text_range_with_text(
            source_file,
            TextRange::new(
                start,
                node.end().expect_source("parsed import bindings have source positions"),
            ),
            "",
        );
    } else {
        // Delete the entire import declaration
        // |import * as ns from './file'|
        // |import { a } from './file'|
        let import_decl = ast::find_ancestor_kind(node, Kind::ImportDeclaration);
        assert!(import_decl.is_some(), "importDecl should not be nil");
        delete_node(
            t,
            source_file,
            import_decl.unwrap(),
            LeadingTriviaOption::IncludeAll,
            TrailingTriviaOption::Include,
        );
    }
}

// delete.go:146
fn delete_variable_declaration(
    t: &mut Tracker,
    deleted_nodes_in_lists: &mut OrderedMap<P<Node>, bool>,
    source_file: P<SourceFile>,
    node: P<Node>,
) {
    let parent = node.parent().unwrap();

    if parent.kind() == Kind::CatchClause {
        // TODO: There's currently no unused diagnostic for this, could be a suggestion
        let open_paren = astnav::find_child_of_kind(parent, Kind::OpenParenToken, source_file);
        let close_paren = astnav::find_child_of_kind(parent, Kind::CloseParenToken, source_file);
        assert!(open_paren.is_some() && close_paren.is_some(), "catch clause should have parens");
        t.delete_node_range(
            source_file,
            open_paren.unwrap(),
            close_paren.unwrap(),
            LeadingTriviaOption::IncludeAll,
            TrailingTriviaOption::Include,
        );
        return;
    }

    if parent.as_variable_declaration_list().declarations.nodes().len() != 1 {
        delete_node_in_list(t, deleted_nodes_in_lists, source_file, node);
        return;
    }

    let gp = parent.parent().unwrap();
    match gp.kind() {
        Kind::ForOfStatement | Kind::ForInStatement => {
            let empty = t
                .node_factory
                .new_object_literal_expression(t.node_factory.new_node_list(vec![]), false);
            t.replace_node(source_file, node, empty, None);
        }

        Kind::ForStatement => delete_node(
            t,
            source_file,
            parent,
            LeadingTriviaOption::IncludeAll,
            TrailingTriviaOption::Include,
        ),

        Kind::VariableStatement => {
            let mut leading_trivia = LeadingTriviaOption::StartLine;
            if has_jsdoc_nodes(Some(gp)) {
                leading_trivia = LeadingTriviaOption::JSDoc;
            }
            delete_node(t, source_file, gp, leading_trivia, TrailingTriviaOption::Include);
        }

        _ => panic!("Unexpected grandparent kind: {:?}", gp.kind()),
    }
}

// delete.go:189
// deleteNode deletes a node with the specified trivia options.
// Warning: This deletes comments too.
fn delete_node(
    t: &mut Tracker,
    source_file: P<SourceFile>,
    node: P<Node>,
    leading_trivia: LeadingTriviaOption,
    trailing_trivia: TrailingTriviaOption,
) {
    let start_position = t.get_adjusted_start_position(source_file, node, leading_trivia, false);
    let end_position = Tracker::get_adjusted_end_position(source_file, node, trailing_trivia);
    t.replace_text_range_with_text(source_file, TextRange::new(start_position, end_position), "");
}

// delete.go:195
fn delete_node_in_list(
    t: &mut Tracker,
    deleted_nodes_in_lists: &mut OrderedMap<P<Node>, bool>,
    source_file: P<SourceFile>,
    node: P<Node>,
) {
    let containing_list = format::get_containing_list(node, source_file);
    assert!(containing_list.is_some(), "containingList should not be nil");
    let containing_list = containing_list.unwrap();
    let index = containing_list.nodes().iter().position(|&n| n == node);
    assert!(index.is_some(), "node should be in containing list");
    let index = index.unwrap();

    if containing_list.nodes().len() == 1 {
        delete_node(
            t,
            source_file,
            node,
            LeadingTriviaOption::IncludeAll,
            TrailingTriviaOption::Include,
        );
        return;
    }

    // Note: We will only delete a comma *after* a node. This will leave a trailing comma if we delete the last node.
    // That's handled in the end by finishTrailingCommaAfterDeletingNodesInList.
    let deleted = deleted_nodes_in_lists.entry(node).or_default();
    assert!(!*deleted, "Deleting a node twice");
    *deleted = true;

    let start_pos = t.start_position_to_delete_node_in_list(source_file, node);
    let end_pos = if index == containing_list.nodes().len() - 1 {
        Tracker::get_adjusted_end_position(source_file, node, TrailingTriviaOption::None)
    } else {
        let prev_node = if index > 0 { Some(containing_list.nodes()[index - 1]) } else { None };
        t.end_position_to_delete_node_in_list(
            source_file,
            node,
            prev_node,
            containing_list.nodes()[index + 1],
        )
    };

    t.replace_text_range_with_text(source_file, TextRange::new(start_pos, end_pos), "");
}

impl Tracker {
    // delete.go:226
    // startPositionToDeleteNodeInList finds the first non-whitespace position in the leading trivia of the node
    pub(crate) fn start_position_to_delete_node_in_list(
        &self,
        source_file: P<SourceFile>,
        node: P<Node>,
    ) -> u32 {
        let start = self.get_adjusted_start_position(
            source_file,
            node,
            LeadingTriviaOption::IncludeAll,
            false,
        );
        scanner::skip_trivia_ex(
            source_file.text(),
            start,
            Some(&scanner::SkipTriviaOptions {
                stop_after_line_break: false,
                stop_at_comments: true,
                ..Default::default()
            }),
        )
    }

    // delete.go:231
    fn end_position_to_delete_node_in_list(
        &self,
        source_file: P<SourceFile>,
        node: P<Node>,
        prev_node: Option<P<Node>>,
        next_node: P<Node>,
    ) -> u32 {
        let end = self.start_position_to_delete_node_in_list(source_file, next_node);
        let Some(prev_node) = prev_node else {
            return end;
        };
        if positions_are_on_same_line(
            Self::get_adjusted_end_position(source_file, node, TrailingTriviaOption::Include),
            end,
            source_file,
        ) {
            return end;
        }
        let token = astnav::find_preceding_token(
            source_file,
            astnav::get_start_of_node(next_node, source_file, false),
        );
        if is_separator(node, token) {
            let token = token.unwrap();
            let prev_token = astnav::find_preceding_token(
                source_file,
                astnav::get_start_of_node(node, source_file, false),
            );
            if is_separator(prev_node, prev_token) {
                let prev_token = prev_token.unwrap();
                let text = source_file.text();
                let pos = scanner::skip_trivia_ex(
                    text,
                    token.end().expect_source("parsed tokens have source positions"),
                    Some(&scanner::SkipTriviaOptions {
                        stop_after_line_break: true,
                        stop_at_comments: true,
                        ..Default::default()
                    }),
                );
                if positions_are_on_same_line(
                    astnav::get_start_of_node(prev_token, source_file, false),
                    astnav::get_start_of_node(token, source_file, false),
                    source_file,
                ) {
                    if pos > 0
                        && stringutil::is_line_break(u32::from(text.as_bytes()[pos as usize - 1]))
                    {
                        return pos - 1;
                    }
                    return pos;
                }
                if stringutil::is_line_break(u32::from(text.as_bytes()[pos as usize])) {
                    return pos;
                }
            }
        }
        end
    }
}

// delete.go:254
pub fn positions_are_on_same_line(pos1: u32, pos2: u32, source_file: P<SourceFile>) -> bool {
    format::get_line_start_position_for_position(pos1, source_file)
        == format::get_line_start_position_for_position(pos2, source_file)
}

// delete.go:258
// hasJSDocNodes checks if a node has JSDoc comments
fn has_jsdoc_nodes(node: Option<P<Node>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    // nil is ok for JSDoc - it will return empty slice if not available
    let jsdocs = node.jsdoc(None);
    !jsdocs.is_empty()
}
