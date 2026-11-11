//! Shared helpers ported from tsgolint's `internal/utils`.

use tsrs_ast::{self as ast, Kind, Node, SourceFile, Symbol, SymbolFlags};
use tsrs_checker::{Checker, SignatureKind, Type, TypeFlags};
use tsrs_compiler::Program;
use tsrs_core::P;
use tsrs_core::tspath::{self, ComparePathsOptions};

use crate::protocol::Range;
use crate::rule::trim_node_range;

pub fn get_await_token_removal_range(source_file: P<SourceFile>, pos: u32) -> Range {
    let range = tsrs_scanner::get_range_of_token_at_position(source_file, pos);
    Range {
        pos: range.source_pos().unwrap(),
        end: tsrs_scanner::skip_trivia_ex(
            source_file.text(),
            range.source_end().unwrap(),
            Some(&tsrs_scanner::SkipTriviaOptions { stop_at_comments: true, ..Default::default() }),
        ),
    }
}

pub fn is_start_of_expression_statement_needing_parentheses(
    source_file: P<SourceFile>,
    node: P<Node>,
    first_token: Kind,
) -> bool {
    if !matches!(first_token, Kind::OpenBraceToken | Kind::ClassKeyword | Kind::FunctionKeyword) {
        return false;
    }
    let start = trim_node_range(source_file, node).pos;
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        if trim_node_range(source_file, parent).pos != start {
            break;
        }
        if parent.kind() == Kind::ExpressionStatement {
            return true;
        }
        ancestor = parent.parent();
    }
    false
}

pub fn is_start_of_arrow_function_body_needing_parentheses(
    source_file: P<SourceFile>,
    mut node: P<Node>,
    first_token: Kind,
) -> bool {
    if first_token != Kind::OpenBraceToken {
        return false;
    }
    while let Some(parent) = node.parent() {
        if parent.kind() == Kind::ParenthesizedExpression {
            return false;
        }
        if parent.kind() == Kind::ArrowFunction && parent.body() == Some(node) {
            return true;
        }
        if trim_node_range(source_file, parent).pos != trim_node_range(source_file, node).pos {
            return false;
        }
        node = parent;
    }
    false
}

pub fn quote_single_string_literal(value: &str) -> String {
    use std::fmt::Write;

    let mut output = String::with_capacity(value.len() + 2);
    output.push('\'');
    for character in value.chars() {
        match character {
            '\t' => output.push_str("\\t"),
            '\u{000B}' => output.push_str("\\v"),
            '\u{000C}' => output.push_str("\\f"),
            '\u{0008}' => output.push_str("\\b"),
            '\r' => output.push_str("\\r"),
            '\n' => output.push_str("\\n"),
            '\\' => output.push_str("\\\\"),
            '\'' => output.push_str("\\'"),
            '\u{2028}' | '\u{2029}' | '\u{0085}' | '\u{0000}'..='\u{001F}' => {
                write!(output, "\\u{:04X}", u32::from(character)).unwrap();
            }
            _ => output.push(character),
        }
    }
    output.push('\'');
    output
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MemberNameType {
    Private,
    Quoted,
    Normal,
    Expression,
}

pub fn get_name_from_member(file: P<SourceFile>, member: P<Node>) -> (String, MemberNameType) {
    match member.kind() {
        Kind::Identifier => return (member.text().into(), MemberNameType::Normal),
        Kind::PrivateIdentifier => return (member.text().into(), MemberNameType::Private),
        Kind::ComputedPropertyName => {
            let expression = member.as_computed_property_name().expression;
            if ast::is_literal_expression(expression) {
                let text = expression.text();
                if !tsrs_scanner::is_valid_identifier(text) {
                    return (format!("\"{text}\""), MemberNameType::Quoted);
                }
                return (text.into(), MemberNameType::Normal);
            }
        }
        _ => {}
    }
    let range = crate::rule::trim_node_range(file, member);
    (file.text()[range.pos as usize..range.end as usize].into(), MemberNameType::Expression)
}

pub fn get_function_head_loc(source_file: P<SourceFile>, node: P<Node>) -> crate::protocol::Range {
    if node.kind() == Kind::ArrowFunction
        && let Some(arrow) = node.as_arrow_function().equals_greater_than_token
    {
        return crate::protocol::Range {
            pos: arrow.pos().expect_source("parsed arrow token"),
            end: arrow.end().expect_source("parsed arrow token"),
        };
    }
    let Some(params) = node.parameter_list() else {
        return crate::rule::trim_node_range(source_file, node);
    };
    let end = params.loc.get().source_pos().unwrap() - 1;
    let mut start = tsrs_scanner::get_token_pos_of_node(node, source_file, false);
    if let Some(modifiers) = node.modifiers()
        && let Some(&modifier) = modifiers.nodes().iter().find(|m| m.kind() != Kind::Decorator)
    {
        start = tsrs_scanner::get_token_pos_of_node(modifier, source_file, false);
    }
    if end < start {
        crate::rule::trim_node_range(source_file, node)
    } else {
        crate::protocol::Range { pos: start, end }
    }
}

pub fn union_type_parts(t: &P<Type>) -> &[P<Type>] {
    if t.flags().intersects(TypeFlags::Union) { t.types() } else { std::slice::from_ref(t) }
}

pub fn intersection_type_parts(t: &P<Type>) -> &[P<Type>] {
    if t.flags().intersects(TypeFlags::Intersection) { t.types() } else { std::slice::from_ref(t) }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TypeAwaitable {
    Always,
    Never,
    May,
}

pub fn needs_to_be_awaited(checker: &mut Checker, node: P<Node>, t: P<Type>) -> TypeAwaitable {
    let is_parameter = t.flags().intersects(TypeFlags::TypeParameter);
    let constraint = if is_parameter { checker.get_base_constraint_of_type(t) } else { Some(t) };
    let Some(constraint) = constraint else {
        return TypeAwaitable::May;
    };
    if constraint.flags().intersects(TypeFlags::Any | TypeFlags::Unknown) {
        return TypeAwaitable::May;
    }
    if is_thenable_type(checker, node, constraint) {
        TypeAwaitable::Always
    } else {
        TypeAwaitable::Never
    }
}

pub fn get_constrained_type_at_location(checker: &mut Checker, node: P<Node>) -> P<Type> {
    let t = checker.get_type_at_location(node);
    checker.get_base_constraint_of_type(t).unwrap_or(t)
}

pub fn is_callback(checker: &mut Checker, parameter: P<Symbol>, node: P<Node>) -> bool {
    let Some(t) = checker.get_type_of_symbol_at_location(parameter, Some(node)) else {
        return false;
    };
    let mut t = checker.get_apparent_type(t);
    if parameter.value_declaration().is_some_and(|declaration| {
        declaration.kind() == Kind::Parameter
            && declaration.as_parameter_declaration().dot_dot_dot_token().is_some()
    }) {
        let Some(index) = checker.get_index_type_of_type(t, checker.number_type) else {
            return false;
        };
        t = index;
    }
    union_type_parts(&t)
        .iter()
        .any(|&part| !checker.get_signatures_of_type(part, SignatureKind::Call).is_empty())
}

pub fn is_thenable_type(checker: &mut Checker, node: P<Node>, t: P<Type>) -> bool {
    let apparent = checker.get_apparent_type(t);
    for &part in union_type_parts(&apparent) {
        let Some(then) = checker.get_property_of_type(part, "then") else {
            continue;
        };
        let Some(then_type) = checker.get_type_of_symbol_at_location(then, Some(node)) else {
            continue;
        };
        for &subtype in union_type_parts(&then_type) {
            for &signature in checker.get_signatures_of_type(subtype, SignatureKind::Call) {
                if let Some(&parameter) = signature.parameters().first()
                    && is_callback(checker, parameter, node)
                {
                    return true;
                }
            }
        }
    }
    false
}

pub fn get_well_known_symbol_property_of_type(
    checker: &mut Checker,
    t: P<Type>,
    name: &str,
) -> Option<P<Symbol>> {
    let name = checker.get_property_name_for_known_symbol_name(name);
    checker.get_property_of_type(t, &name)
}

pub fn is_source_file_default_library(program: &Program, file: P<SourceFile>) -> bool {
    if !file.is_declaration_file() {
        return false;
    }
    if program.is_source_file_default_library(file.path()) {
        return true;
    }
    let options = program.options();
    if options.no_lib.is_true() {
        return false;
    }
    let library_path = program.host().default_library_path();
    let libs = match &options.lib {
        None => vec![tspath::combine_paths(
            library_path,
            &[tsrs_tsoptions::get_default_lib_file_name(&options)],
        )],
        Some(libs) => libs
            .iter()
            .filter_map(|lib| tsrs_tsoptions::get_lib_file_name(lib))
            .map(|name| tspath::combine_paths(library_path, &[&name]))
            .collect(),
    };
    libs.iter().any(|lib| {
        tspath::compare_paths(
            file.file_name(),
            lib,
            &ComparePathsOptions {
                current_directory: program.host().get_current_directory().into(),
                use_case_sensitive_file_names: program.host().fs().use_case_sensitive_file_names(),
            },
        )
        .is_eq()
    })
}

pub fn is_symbol_from_default_library(program: &Program, symbol: P<Symbol>) -> bool {
    symbol.declarations().iter().any(|&declaration| {
        ast::get_source_file_of_node(declaration)
            .is_some_and(|file| is_source_file_default_library(program, file))
    })
}

pub fn is_builtin_symbol_like(
    program: &Program,
    checker: &mut Checker,
    t: P<Type>,
    names: &[&str],
) -> bool {
    is_builtin_symbol_like_recurser(checker, t, &mut |_, subtype| {
        let Some(symbol) = subtype.symbol() else {
            return Some(false);
        };
        (names.contains(&symbol.name()) && is_symbol_from_default_library(program, symbol))
            .then_some(true)
    })
}

pub fn is_any_builtin_symbol_like(program: &Program, checker: &mut Checker, t: P<Type>) -> bool {
    is_builtin_symbol_like_recurser(checker, t, &mut |_, subtype| {
        let Some(symbol) = subtype.symbol() else {
            return Some(false);
        };
        is_symbol_from_default_library(program, symbol).then_some(true)
    })
}

pub fn is_readonly_error_like(program: &Program, checker: &mut Checker, t: P<Type>) -> bool {
    is_builtin_symbol_like_recurser(checker, t, &mut |checker, subtype| {
        let Some(alias) = subtype.alias() else {
            return Some(false);
        };
        let Some(&argument) = alias.type_arguments().first() else {
            return Some(false);
        };
        let symbol = alias.symbol();
        if symbol.is_some_and(|symbol| {
            symbol.name() == "Readonly" && is_symbol_from_default_library(program, symbol)
        }) && (is_builtin_symbol_like(program, checker, argument, &["Error"])
            || is_readonly_error_like(program, checker, argument))
        {
            return Some(true);
        }
        None
    })
}

fn is_builtin_symbol_like_recurser(
    checker: &mut Checker,
    t: P<Type>,
    predicate: &mut impl FnMut(&mut Checker, P<Type>) -> Option<bool>,
) -> bool {
    if t.flags().intersects(TypeFlags::Intersection) {
        return t
            .types()
            .iter()
            .any(|&part| is_builtin_symbol_like_recurser(checker, part, predicate));
    }
    if t.flags().intersects(TypeFlags::Union) {
        return t
            .types()
            .iter()
            .all(|&part| is_builtin_symbol_like_recurser(checker, part, predicate));
    }
    if t.flags().intersects(TypeFlags::TypeParameter) {
        return checker.get_base_constraint_of_type(t).is_some_and(|constraint| {
            is_builtin_symbol_like_recurser(checker, constraint, predicate)
        });
    }
    if let Some(result) = predicate(checker, t) {
        return result;
    }
    if let Some(symbol) = t.symbol()
        && symbol.flags().intersects(SymbolFlags::Class | SymbolFlags::Interface)
    {
        let declared = checker.get_declared_type_of_symbol(symbol);
        for &base in checker.get_base_types(declared) {
            if is_builtin_symbol_like_recurser(checker, base, predicate) {
                return true;
            }
        }
    }
    false
}

pub fn get_type_name(checker: &mut Checker, t: P<Type>) -> std::borrow::Cow<'static, str> {
    if t.flags().intersects(TypeFlags::StringLike) {
        return std::borrow::Cow::Borrowed("string");
    }
    if t.flags().intersects(TypeFlags::TypeParameter)
        && let Some(declaration) =
            t.symbol().and_then(|symbol| symbol.declarations().first().copied())
        && declaration.kind() == Kind::TypeParameter
        && let Some(constraint) = declaration.as_type_parameter_declaration().constraint
    {
        let constraint = checker.get_type_from_type_node(constraint);
        return get_type_name(checker, constraint);
    }
    if t.flags().intersects(TypeFlags::Union)
        && t.types().iter().all(|&part| get_type_name(checker, part) == "string")
        || t.flags().intersects(TypeFlags::Intersection)
            && t.types().iter().any(|&part| get_type_name(checker, part) == "string")
    {
        return std::borrow::Cow::Borrowed("string");
    }
    std::borrow::Cow::Owned(checker.type_to_string_exported(t))
}

pub fn matches_type_or_base_type(
    checker: &mut Checker,
    t: P<Type>,
    predicate: &mut impl FnMut(&mut Checker, P<Type>) -> bool,
) -> bool {
    if predicate(checker, t) {
        return true;
    }
    if !t.flags().intersects(TypeFlags::Object) {
        return false;
    }
    let target = if t.object_flags().intersects(tsrs_checker::ObjectFlags::Reference) {
        t.target().unwrap()
    } else {
        t
    };
    target.object_flags().intersects(tsrs_checker::ObjectFlags::ClassOrInterface)
        && checker
            .get_base_types(target)
            .iter()
            .any(|&base| matches_type_or_base_type(checker, base, predicate))
}

pub fn type_recurser(t: P<Type>, predicate: &mut impl FnMut(P<Type>) -> bool) -> bool {
    if t.flags().intersects(TypeFlags::UnionOrIntersection) {
        t.types().iter().any(|&part| type_recurser(part, predicate))
    } else {
        predicate(t)
    }
}

pub fn get_for_statement_head_loc(file: P<SourceFile>, node: P<Node>) -> crate::protocol::Range {
    let statement = if node.kind() == Kind::ForStatement {
        node.as_for_statement().statement()
    } else {
        node.as_for_in_or_of_statement().statement
    };
    let mut range = crate::rule::trim_node_range(file, node);
    range.end = statement.pos().expect_source("parsed lint node");
    range
}

pub fn is_array_method_call_with_predicate(checker: &mut Checker, node: P<Node>) -> bool {
    let callee = node.as_call_expression().expression;
    if !ast::is_access_expression(callee) {
        return false;
    }
    let (name, known) = checker.get_accessed_property_name(callee);
    if !known
        || !["every", "filter", "find", "findIndex", "findLast", "findLastIndex", "some"]
            .contains(&name.as_ref())
    {
        return false;
    }
    let t = get_constrained_type_at_location(checker, callee.expression().unwrap());
    type_recurser(t, &mut |t| checker.is_array_or_tuple_type(t))
}

pub fn get_heritage_clauses(node: P<Node>) -> Option<P<ast::NodeList>> {
    match node.kind() {
        Kind::ClassDeclaration => node.as_class_declaration().heritage_clauses(),
        Kind::ClassExpression => node.as_class_expression().heritage_clauses(),
        Kind::InterfaceDeclaration => node.as_interface_declaration().heritage_clauses(),
        _ => None,
    }
}

pub fn is_rest_parameter_declaration(node: P<Node>) -> bool {
    node.kind() == Kind::Parameter && node.as_parameter_declaration().dot_dot_dot_token().is_some()
}

pub fn has_comments_in_range(file: P<SourceFile>, range: crate::protocol::Range) -> bool {
    tsrs_scanner::get_trailing_comment_ranges(file.text(), range.pos)
        .take_while(|comment| comment.pos() < range.end)
        .chain(
            tsrs_scanner::get_leading_comment_ranges(file.text(), range.pos)
                .take_while(|comment| comment.pos() < range.end),
        )
        .any(|comment| comment.pos() < range.end)
}

pub fn is_str_white_space(character: char) -> bool {
    matches!(
        character,
        '\n' | '\r'
            | '\u{2028}'
            | '\u{2029}'
            | '\t'
            | '\u{b}'
            | '\u{c}'
            | '\u{feff}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

pub fn is_strong_precedence_node(node: P<Node>) -> bool {
    ast::is_literal_kind(node.kind())
        || matches!(
            node.kind(),
            Kind::TrueKeyword
                | Kind::FalseKeyword
                | Kind::ParenthesizedExpression
                | Kind::Identifier
                | Kind::TypeReference
                | Kind::TypeOperator
                | Kind::ArrayLiteralExpression
                | Kind::ObjectLiteralExpression
                | Kind::PropertyAccessExpression
                | Kind::ElementAccessExpression
                | Kind::CallExpression
                | Kind::NewExpression
                | Kind::TaggedTemplateExpression
                | Kind::ExpressionWithTypeArguments
        )
}

pub fn is_intrinsic_error_type(ty: P<Type>) -> bool {
    ty.flags().intersects(TypeFlags::Intrinsic)
        && ty.as_intrinsic_type().intrinsic_name() == "error"
}

pub fn is_unsafe_assignment(
    sender: P<Type>,
    receiver: P<Type>,
    checker: &mut Checker,
    sender_node: Option<P<Node>>,
) -> Option<(P<Type>, P<Type>)> {
    fn visit(
        sender: P<Type>,
        receiver: P<Type>,
        checker: &mut Checker,
        sender_node: Option<P<Node>>,
        visited: &mut rustc_hash::FxHashSet<(P<Type>, P<Type>)>,
    ) -> bool {
        if sender.flags().intersects(TypeFlags::Any) {
            if receiver.flags().intersects(TypeFlags::Unknown) {
                return false;
            }
            if !receiver.flags().intersects(TypeFlags::Any) {
                return true;
            }
        }
        if !visited.insert((sender, receiver)) {
            return false;
        }
        if tsrs_checker::is_non_deferred_type_reference(sender)
            && tsrs_checker::is_non_deferred_type_reference(receiver)
        {
            if sender.target() != receiver.target() {
                return false;
            }
            if sender_node.is_some_and(|node| {
                node.kind() == Kind::NewExpression
                    && node.expression().is_some_and(|expression| {
                        expression.kind() == Kind::Identifier && expression.text() == "Map"
                    })
                    && node.arguments().is_empty()
                    && node.as_new_expression().type_arguments().is_none()
            }) {
                return false;
            }
            let sender_arguments = checker.get_type_arguments(sender);
            let receiver_arguments = checker.get_type_arguments(receiver);
            for (index, &argument) in sender_arguments.iter().enumerate() {
                if visit(argument, receiver_arguments[index], checker, sender_node, visited) {
                    return true;
                }
            }
        }
        false
    }
    visit(sender, receiver, checker, sender_node, &mut rustc_hash::FxHashSet::default())
        .then_some((receiver, sender))
}

pub fn get_contextual_type(checker: &mut Checker, node: P<Node>) -> Option<P<Type>> {
    let parent = node.parent()?;
    match parent.kind() {
        Kind::CallExpression | Kind::NewExpression => {
            if parent.expression() == Some(node) {
                return None;
            }
        }
        Kind::VariableDeclaration | Kind::PropertyDeclaration | Kind::Parameter => {
            return parent.type_node().map(|ty| checker.get_type_from_type_node_exported(ty));
        }
        Kind::JsxExpression => {
            return checker.get_contextual_type(parent, tsrs_checker::ContextFlags::None);
        }
        Kind::PropertyAssignment | Kind::ShorthandPropertyAssignment
            if node.kind() == Kind::Identifier =>
        {
            return checker.get_contextual_type(node, tsrs_checker::ContextFlags::None);
        }
        Kind::BinaryExpression
            if parent.as_binary_expression().operator_token.kind() == Kind::EqualsToken
                && parent.as_binary_expression().right() == node =>
        {
            return Some(checker.get_type_at_location(parent.as_binary_expression().left));
        }
        Kind::TemplateSpan => {}
        _ => return None,
    }
    checker.get_contextual_type(node, tsrs_checker::ContextFlags::None)
}

pub fn get_this_expression(mut node: P<Node>) -> Option<P<Node>> {
    loop {
        node = ast::skip_parentheses(node);
        match node.kind() {
            Kind::ThisKeyword => return Some(node),
            Kind::CallExpression
            | Kind::PropertyAccessExpression
            | Kind::ElementAccessExpression => node = node.expression().unwrap(),
            _ => return None,
        }
    }
}

pub fn get_enum_literals(ty: P<Type>) -> Vec<P<Type>> {
    union_type_parts(&ty)
        .iter()
        .copied()
        .filter(|part| part.flags().intersects(TypeFlags::EnumLiteral))
        .collect()
}

pub fn get_enum_types(checker: &mut Checker, ty: P<Type>) -> Vec<P<Type>> {
    get_enum_literals(ty)
        .into_iter()
        .map(|ty| {
            match ty.symbol().filter(|symbol| symbol.flags().intersects(SymbolFlags::EnumMember)) {
                Some(symbol) => checker.get_type_at_location(
                    symbol
                        .value_declaration()
                        .expect("enum member value declaration")
                        .parent()
                        .expect("enum declaration"),
                ),
                None => ty,
            }
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DiscriminatedAnyType {
    Any,
    PromiseAny,
    AnyArray,
    Safe,
}

pub fn discriminate_any_type(
    checker: &mut Checker,
    ty: P<Type>,
    node: P<Node>,
) -> DiscriminatedAnyType {
    fn worker(
        checker: &mut Checker,
        ty: P<Type>,
        node: P<Node>,
        visited: &mut rustc_hash::FxHashSet<P<Type>>,
    ) -> DiscriminatedAnyType {
        if !visited.insert(ty) {
            return DiscriminatedAnyType::Safe;
        }
        if ty.flags().intersects(TypeFlags::Any) {
            return DiscriminatedAnyType::Any;
        }
        if checker.is_array_type(ty)
            && checker.get_type_arguments(ty)[0].flags().intersects(TypeFlags::Any)
        {
            return DiscriminatedAnyType::AnyArray;
        }
        if type_recurser(ty, &mut |part| {
            if !is_thenable_type(checker, node, part) {
                return false;
            }
            let Some(awaited) = checker.get_awaited_type(part) else {
                return false;
            };
            worker(checker, awaited, node, visited) == DiscriminatedAnyType::Any
        }) {
            DiscriminatedAnyType::PromiseAny
        } else {
            DiscriminatedAnyType::Safe
        }
    }
    worker(checker, ty, node, &mut rustc_hash::FxHashSet::default())
}

pub fn collect_all_call_signatures(
    checker: &mut Checker,
    ty: P<Type>,
) -> Vec<P<tsrs_checker::Signature>> {
    if ty.flags().intersects(TypeFlags::Union) {
        let mut signatures = Vec::new();
        for &part in ty.types() {
            signatures.extend_from_slice(checker.get_signatures_of_type(part, SignatureKind::Call));
        }
        return signatures;
    }
    if ty.flags().intersects(TypeFlags::Intersection) {
        let mut signatures = Vec::new();
        for &part in ty.types() {
            let part_signatures = checker.get_signatures_of_type(part, SignatureKind::Call);
            if !part_signatures.is_empty() {
                if !signatures.is_empty() {
                    return Vec::new();
                }
                signatures.extend_from_slice(part_signatures);
            }
        }
        return signatures;
    }
    checker.get_signatures_of_type(ty, SignatureKind::Call).to_vec()
}
