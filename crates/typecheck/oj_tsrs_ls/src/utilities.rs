use rustc_hash::FxHashSet;
use std::rc::Rc;
use tsrs_ast::{
    self as ast, CommentRange, FileReference, Kind, ModifierFlags, Node, NodeFlags, NodeList,
    SemanticMeaning, SourceFile, Symbol, SymbolFlags,
};
use tsrs_checker::{
    Checker, ContextFlags, LiteralValue, Signature, SignatureKind, Type, TypeFlags,
};
use tsrs_compiler::Program;
use tsrs_core::jsnum::{self, PseudoBigInt};
use tsrs_core::{P, TextPos, TextRange, debug, stringutil, tspath};
use tsrs_lsproto as lsproto;
use tsrs_scanner as scanner;

use crate::astnav;
use crate::completions::{get_switched_type, is_equality_operator_kind};
use crate::findallreferences::{RefInfo, get_range_of_node};
use crate::languageservice::LanguageService;
use crate::lsformat::get_range_of_enclosing_comment;
use crate::lsutil::{self, QuotePreference, UserPreferences};
use crate::spanmap::{Feature, Fidelity};

// utilities.go:26 (strings.NewReplacer("'", `\'`, `\"`, `"`): one left-to-right pass)
fn quote_replacer_replace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('\'') {
            out.push_str("\\'");
            rest = r;
        } else if let Some(r) = rest.strip_prefix("\\\"") {
            out.push('"');
            rest = r;
        } else {
            let ch = rest.chars().next().unwrap();
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    out
}

// utilities.go:28
pub fn is_in_string(
    source_file: P<SourceFile>,
    position: u32,
    previous_token: Option<P<Node>>,
) -> bool {
    if let Some(previous_token) = previous_token
        && ast::is_string_text_containing_node(previous_token)
    {
        let start =
            astnav::get_start_of_node(previous_token, source_file, false /*includeJSDoc*/);
        let end = previous_token.end().expect_source("parsed tokens have source positions");

        // To be "in" one of these literals, the position has to be:
        //   1. entirely within the token text.
        //   2. at the end position of an unterminated token.
        //   3. at the end of a regular expression (due to trailing flags like '/foo/g').
        if start < position && position < end {
            return true;
        }

        if position == end {
            return ast::is_unterminated_literal(previous_token);
        }
    }
    false
}

// utilities.go:47
pub fn is_module_specifier_like(node: P<Node>) -> bool {
    if !ast::is_string_literal_like(node) {
        return false;
    }

    let parent = node.parent().unwrap();
    if ast::is_require_call(parent, false /*requireStringLiteralLikeArgument*/)
        || ast::is_import_call(parent)
    {
        return parent.arguments()[0] == node;
    }

    parent.kind() == Kind::ExternalModuleReference
        || parent.kind() == Kind::ImportDeclaration
        || parent.kind() == Kind::JSImportDeclaration
}

// utilities.go:61
pub fn get_non_module_symbol_of_merged_module_symbol(symbol: P<Symbol>) -> Option<P<Symbol>> {
    if symbol.declarations().is_empty()
        || !symbol.flags().intersects(SymbolFlags::Module | SymbolFlags::Transient)
    {
        return None;
    }

    if let Some(decl) = symbol
        .declarations()
        .iter()
        .copied()
        .find(|&d| !ast::is_source_file(d) && !ast::is_module_declaration(d))
    {
        return decl.symbol();
    }
    None
}

// utilities.go:72
pub fn get_local_symbol_for_export_specifier(
    reference_location: P<Node>,
    reference_symbol: Option<P<Symbol>>,
    export_specifier: P<Node>,
    ch: &mut Checker,
) -> Option<P<Symbol>> {
    if is_export_specifier_alias(reference_location, export_specifier)
        && let Some(symbol) = ch.get_export_specifier_local_target_symbol(export_specifier)
    {
        return Some(symbol);
    }
    reference_symbol
}

// utilities.go:81
pub fn is_export_specifier_alias(reference_location: P<Node>, export_specifier: P<Node>) -> bool {
    let spec = export_specifier.as_export_specifier();
    debug::assert(
        spec.property_name == Some(reference_location)
            || export_specifier.name() == Some(reference_location),
        &[&"referenceLocation is not export specifier name or property name"],
    );
    if let Some(property_name) = spec.property_name {
        // Given `export { foo as bar } [from "someModule"]`: It's an alias at `foo`, but at `bar` it's a new symbol.
        property_name == reference_location
    } else {
        // `export { foo } from "foo"` is a re-export.
        // `export { foo };` is not a re-export, it creates an alias for the local variable `foo`.
        export_specifier.parent().unwrap().parent().unwrap().module_specifier().is_none()
    }
}

// utilities.go:95
pub fn is_in_comment(
    file: P<SourceFile>,
    position: u32,
    token_at_position: P<Node>,
) -> Option<CommentRange> {
    get_range_of_enclosing_comment(
        file,
        position,
        astnav::find_preceding_token(file, position),
        token_at_position,
    )
}

// utilities.go:99
pub fn position_belongs_to_node(candidate: P<Node>, position: u32, file: P<SourceFile>) -> bool {
    lsutil::position_belongs_to_node(candidate, position, file)
}

// utilities.go:103
pub struct PossibleTypeArgumentInfo {
    pub(crate) called: P<Node>,
    pub(crate) n_type_arguments: usize,
}

// Get info for an expression like `f <` that may be the start of type arguments.
// utilities.go:109
pub fn get_possible_type_arguments_info(
    token_in: Option<P<Node>>,
    source_file: P<SourceFile>,
) -> Option<PossibleTypeArgumentInfo> {
    // This is a rare case, but one that saves on a _lot_ of work if true - if the source file has _no_ `<` character,
    // then there obviously can't be any type arguments - no expensive brace-matching backwards scanning required
    if !source_file.text().as_bytes().contains(&b'<') {
        return None;
    }

    let mut token = token_in;
    // This function determines if the node could be a type argument position
    // When editing, it is common to have an incomplete type argument list (e.g. missing ">"),
    // so the tree can have any shape depending on the tokens before the current node.
    // Instead, scanning for an identifier followed by a "<" before current node
    // will typically give us better results than inspecting the tree.
    // Note that we also balance out the already provided type arguments, arrays, object literals while doing so.
    let mut remaining_less_than_tokens = 0;
    let mut n_type_arguments = 0;
    while let Some(mut t) = token {
        match t.kind() {
            Kind::LessThanToken => {
                // Found the beginning of the generic argument expression
                let mut tok = astnav::find_preceding_token(
                    source_file,
                    t.pos().expect_source("parsed tokens have source positions"),
                );
                if let Some(q) = tok
                    && q.kind() == Kind::QuestionDotToken
                {
                    tok = astnav::find_preceding_token(
                        source_file,
                        q.pos().expect_source("parsed tokens have source positions"),
                    );
                }
                let ident = tok?;
                if !ast::is_identifier(ident) {
                    return None;
                }
                if remaining_less_than_tokens == 0 {
                    if ast::is_declaration_name(ident) {
                        return None;
                    }
                    return Some(PossibleTypeArgumentInfo { called: ident, n_type_arguments });
                }
                remaining_less_than_tokens -= 1;
                t = ident;
            }
            Kind::GreaterThanGreaterThanGreaterThanToken => remaining_less_than_tokens += 3,
            Kind::GreaterThanGreaterThanToken => remaining_less_than_tokens += 2,
            Kind::GreaterThanToken => remaining_less_than_tokens += 1,
            Kind::CloseBraceToken => {
                // This can be object type, skip until we find the matching open brace token
                // Skip until the matching open brace token
                t = find_preceding_matching_token(t, Kind::OpenBraceToken, source_file)?;
            }
            Kind::CloseParenToken => {
                // This can be object type, skip until we find the matching open brace token
                // Skip until the matching open brace token
                t = find_preceding_matching_token(t, Kind::OpenParenToken, source_file)?;
            }
            Kind::CloseBracketToken => {
                // This can be object type, skip until we find the matching open brace token
                // Skip until the matching open brace token
                t = find_preceding_matching_token(t, Kind::OpenBracketToken, source_file)?;
            }
            Kind::CommaToken => {
                // Valid tokens in a type name. Skip.
                n_type_arguments += 1;
            }
            Kind::EqualsGreaterThanToken
            | Kind::Identifier
            | Kind::StringLiteral
            | Kind::NumericLiteral
            | Kind::BigIntLiteral
            | Kind::TrueKeyword
            | Kind::FalseKeyword
            | Kind::TypeOfKeyword
            | Kind::ExtendsKeyword
            | Kind::KeyOfKeyword
            | Kind::DotToken
            | Kind::BarToken
            | Kind::QuestionToken
            | Kind::ColonToken => {
                // do nothing
            }
            _ => {
                if !ast::is_type_node(t) {
                    // Invalid token in type
                    return None;
                }
            }
        }
        token = astnav::find_preceding_token(
            source_file,
            t.pos().expect_source("parsed tokens have source positions"),
        );
    }
    None
}

// utilities.go:188
pub fn is_name_of_module_declaration(node: P<Node>) -> bool {
    let parent = node.parent().unwrap();
    if parent.kind() != Kind::ModuleDeclaration {
        return false;
    }
    parent.name() == Some(node)
}

// utilities.go:195
pub fn is_expression_of_external_module_import_equals_declaration(node: P<Node>) -> bool {
    let grand_parent = node.parent().unwrap().parent().unwrap();
    ast::is_external_module_import_equals_declaration(grand_parent)
        && ast::get_external_module_import_equals_declaration_expression(grand_parent) == Some(node)
}

// utilities.go:199
pub fn is_namespace_reference(node: P<Node>) -> bool {
    is_qualified_name_namespace_reference(node) || is_property_access_namespace_reference(node)
}

// utilities.go:203
pub fn is_qualified_name_namespace_reference(node: P<Node>) -> bool {
    let mut root = node;
    let mut is_last_clause = true;
    if root.parent().unwrap().kind() == Kind::QualifiedName {
        while let Some(parent) = root.parent() {
            if parent.kind() != Kind::QualifiedName {
                break;
            }
            root = parent;
        }

        is_last_clause = root.as_qualified_name().right == node;
    }

    root.parent().unwrap().kind() == Kind::TypeReference && !is_last_clause
}

// utilities.go:217
pub fn is_property_access_namespace_reference(node: P<Node>) -> bool {
    let mut root = node;
    let mut is_last_clause = true;
    if root.parent().unwrap().kind() == Kind::PropertyAccessExpression {
        while let Some(parent) = root.parent() {
            if parent.kind() != Kind::PropertyAccessExpression {
                break;
            }
            root = parent;
        }

        is_last_clause = root.name() == Some(node);
    }

    let root_parent = root.parent().unwrap();
    if !is_last_clause
        && root_parent.kind() == Kind::ExpressionWithTypeArguments
        && root_parent.parent().unwrap().kind() == Kind::HeritageClause
    {
        let heritage_clause = root_parent.parent().unwrap();
        let decl = heritage_clause.parent().unwrap();
        return (decl.kind() == Kind::ClassDeclaration
            && heritage_clause.as_heritage_clause().token == Kind::ImplementsKeyword)
            || (decl.kind() == Kind::InterfaceDeclaration
                && heritage_clause.as_heritage_clause().token == Kind::ExtendsKeyword);
    }

    false
}

// utilities.go:238
pub fn is_this(node: P<Node>) -> bool {
    match node.kind() {
        Kind::ThisKeyword => {
            // case ast.KindThisType: TODO: GH#9267
            true
        }
        Kind::Identifier => {
            // 'this' as a parameter
            node.text() == "this" && node.parent().unwrap().kind() == Kind::Parameter
        }
        _ => false,
    }
}

// utilities.go:251
pub fn is_type_reference(mut node: P<Node>) -> bool {
    if ast::is_right_side_of_qualified_name_or_property_access(node) {
        node = node.parent().unwrap();
    }

    match node.kind() {
        Kind::ThisKeyword => return !ast::is_expression_node(node),
        Kind::ThisType => return true,
        _ => {}
    }

    let parent = node.parent().unwrap();
    match parent.kind() {
        Kind::TypeReference => return true,
        Kind::ImportType => return !parent.as_import_type_node().is_type_of,
        Kind::ExpressionWithTypeArguments => return ast::is_part_of_type_node(parent),
        _ => {}
    }

    false
}

// utilities.go:274
pub fn is_in_right_side_of_internal_import_equals_declaration(mut node: P<Node>) -> bool {
    if node.parent().is_none() {
        return false;
    }
    while node.parent().unwrap().kind() == Kind::QualifiedName {
        node = node.parent().unwrap();
    }

    let parent = node.parent().unwrap();
    ast::is_internal_module_import_equals_declaration(parent)
        && parent.as_import_equals_declaration().module_reference == node
}

impl LanguageService {
    // utilities.go:285
    pub(crate) fn create_lsp_range_from_node(
        &self,
        node: P<Node>,
        file: P<SourceFile>,
    ) -> (lsproto::Range, Fidelity) {
        self.create_lsp_range_from_bounds(
            scanner::get_token_pos_of_node(node, file, false /*includeJSDoc*/),
            node.end().expect_source("parsed nodes have source positions"),
            file,
        )
    }

    // utilities.go:289
    pub(crate) fn create_lsp_range_from_node_for_feature(
        &self,
        node: P<Node>,
        file: P<SourceFile>,
        feature: Feature,
    ) -> (lsproto::Range, Fidelity) {
        self.converters.to_lsp_range_for_feature(&file, create_range_from_node(node, file), feature)
    }
}

// utilities.go:293
pub fn create_range_from_node(node: P<Node>, file: P<SourceFile>) -> TextRange {
    TextRange::new(
        scanner::get_token_pos_of_node(node, file, false /*includeJSDoc*/),
        node.end().expect_source("parsed nodes have source positions"),
    )
}

impl LanguageService {
    // utilities.go:297
    pub(crate) fn create_lsp_range_from_bounds(
        &self,
        start: u32,
        end: u32,
        file: P<SourceFile>,
    ) -> (lsproto::Range, Fidelity) {
        self.converters.to_lsp_range(&file, TextRange::new(start, end))
    }

    // utilities.go:301
    pub(crate) fn create_lsp_range_from_range(
        &self,
        text_range: TextRange,
        script: &dyn crate::lsconv::Script,
    ) -> (lsproto::Range, Fidelity) {
        self.converters.to_lsp_range(script, text_range)
    }

    // utilities.go:305
    pub(crate) fn create_lsp_position(
        &self,
        position: u32,
        file: P<SourceFile>,
    ) -> (lsproto::Position, Fidelity) {
        self.converters.to_lsp_position(&file, TextPos::source(position))
    }
}

// utilities.go:309
pub fn quote(file: P<SourceFile>, preferences: &UserPreferences, text: &str) -> String {
    // Editors can pass in undefined or empty string - we want to infer the preference in those cases.
    let quote_preference = lsutil::get_quote_preference(file, preferences);
    let mut quoted = tsrs_core::json::marshal_indent(
        &tsrs_core::json::Value::String(text.to_string()),
        "", /*prefix*/
        "", /*indent*/
    )
    .unwrap_or_default();
    if quote_preference == QuotePreference::Single {
        quoted = format!("'{}'", quote_replacer_replace(stringutil::strip_quotes(&quoted)));
    }
    quoted
}

// utilities.go:319
const TYPE_KEYWORDS: &[Kind] = &[
    Kind::AnyKeyword,
    Kind::AssertsKeyword,
    Kind::BigIntKeyword,
    Kind::BooleanKeyword,
    Kind::FalseKeyword,
    Kind::InferKeyword,
    Kind::KeyOfKeyword,
    Kind::NeverKeyword,
    Kind::NullKeyword,
    Kind::NumberKeyword,
    Kind::ObjectKeyword,
    Kind::ReadonlyKeyword,
    Kind::StringKeyword,
    Kind::SymbolKeyword,
    Kind::TypeOfKeyword,
    Kind::TrueKeyword,
    Kind::VoidKeyword,
    Kind::UndefinedKeyword,
    Kind::UniqueKeyword,
    Kind::UnknownKeyword,
];

// utilities.go:342
pub fn is_type_keyword(kind: Kind) -> bool {
    TYPE_KEYWORDS.contains(&kind)
}

// utilities.go:350
pub fn is_literal_name_of_property_declaration_or_index_access(node: P<Node>) -> bool {
    // utilities
    let parent = node.parent().unwrap();
    match parent.kind() {
        Kind::PropertyDeclaration
        | Kind::PropertySignature
        | Kind::PropertyAssignment
        | Kind::EnumMember
        | Kind::MethodDeclaration
        | Kind::MethodSignature
        | Kind::GetAccessor
        | Kind::SetAccessor
        | Kind::ModuleDeclaration => ast::get_name_of_declaration(parent) == Some(node),
        Kind::ElementAccessExpression => {
            parent.as_element_access_expression().argument_expression == node
        }
        Kind::ComputedPropertyName => true,
        Kind::LiteralType => parent.parent().unwrap().kind() == Kind::IndexedAccessType,
        _ => false,
    }
}

// utilities.go:374
pub fn is_object_binding_element_without_property_name(binding_element: P<Node>) -> bool {
    binding_element.kind() == Kind::BindingElement
        && binding_element.parent().unwrap().kind() == Kind::ObjectBindingPattern
        && binding_element.name().unwrap().kind() == Kind::Identifier
        && binding_element.property_name().is_none()
}

// utilities.go:381
pub fn is_right_side_of_property_access(node: P<Node>) -> bool {
    match node.parent() {
        Some(parent) => {
            parent.kind() == Kind::PropertyAccessExpression && parent.name() == Some(node)
        }
        None => false,
    }
}

// utilities.go:385
pub fn is_static_symbol(symbol: P<Symbol>) -> bool {
    let Some(value_declaration) = symbol.value_declaration() else {
        return false;
    };
    let modifier_flags = value_declaration.modifier_flags();
    modifier_flags.intersects(ModifierFlags::Static)
}

// utilities.go:393
pub fn is_implementation(node: P<Node>) -> bool {
    if node.flags().intersects(NodeFlags::Ambient) {
        return !(node.kind() == Kind::InterfaceDeclaration
            || node.kind() == Kind::TypeAliasDeclaration);
    }
    if ast::is_variable_like(node) {
        return ast::has_initializer(node);
    }
    if ast::is_function_like_declaration(node) {
        return node.body().is_some();
    }
    ast::is_class_like(node) || ast::is_module_or_enum_declaration(node)
}

// utilities.go:409
pub fn is_implementation_expression(node: P<Node>) -> bool {
    match node.kind() {
        Kind::ParenthesizedExpression => is_implementation_expression(node.expression().unwrap()),
        Kind::ArrowFunction
        | Kind::FunctionExpression
        | Kind::ObjectLiteralExpression
        | Kind::ClassExpression
        | Kind::ArrayLiteralExpression => true,
        _ => false,
    }
}

// utilities.go:420
pub fn is_readonly_type_operator(node: P<Node>) -> bool {
    node.kind() == Kind::ReadonlyKeyword
        && node.parent().unwrap().kind() == Kind::TypeOperator
        && node.parent().unwrap().as_type_operator_node().operator == Kind::ReadonlyKeyword
}

// utilities.go:424
pub fn is_jump_statement_target(node: P<Node>) -> bool {
    node.kind() == Kind::Identifier
        && ast::is_break_or_continue_statement(node.parent().unwrap())
        && node.parent().unwrap().label() == Some(node)
}

// utilities.go:428
pub fn is_label_of_labeled_statement(node: P<Node>) -> bool {
    node.kind() == Kind::Identifier
        && node.parent().unwrap().kind() == Kind::LabeledStatement
        && node.parent().unwrap().label() == Some(node)
}

// utilities.go:432
pub fn find_reference_in_position(refs: &[P<FileReference>], pos: u32) -> Option<P<FileReference>> {
    refs.iter().copied().find(|r| r.text_range.contains_inclusive(pos.into()))
}

// utilities.go:436
pub fn get_containing_node_if_in_heritage_clause(node: P<Node>) -> Option<P<Node>> {
    if node.kind() == Kind::Identifier
        || node.kind() == Kind::QualifiedName
        || node.kind() == Kind::PropertyAccessExpression
    {
        return get_containing_node_if_in_heritage_clause(node.parent().unwrap());
    }
    if node.kind() == Kind::ExpressionWithTypeArguments || node.kind() == Kind::TypeReference {
        let parent = node.parent().unwrap();
        if ast::is_heritage_clause(parent) {
            let grand_parent = parent.parent().unwrap();
            if ast::is_class_like(grand_parent) || grand_parent.kind() == Kind::InterfaceDeclaration
            {
                return Some(grand_parent);
            }
        }
    }
    None
}

// utilities.go:448
pub fn get_container_node(node: P<Node>) -> Option<P<Node>> {
    let parent = node.parent();
    if let Some(result) = std::iter::successors(parent, |p| p.parent()).find_map(|p| {
        match p.kind() {
            Kind::SourceFile
            | Kind::MethodDeclaration
            | Kind::MethodSignature
            | Kind::FunctionDeclaration
            | Kind::FunctionExpression
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::ClassDeclaration
            | Kind::InterfaceDeclaration
            | Kind::EnumDeclaration
            | Kind::ModuleDeclaration => return Some(Some(p)),
            _ => {}
        }

        None
    }) {
        return result;
    }
    None
}

// utilities.go:459
pub fn get_adjusted_location(
    node: P<Node>,
    for_rename: bool,
    mut source_file: Option<P<SourceFile>>,
) -> P<Node> {
    // todo: check if this function needs to be changed for jsdoc updates

    // Go reads `node.Parent` only after checking that `node` is a keyword (keywords always have a parent), so a
    // parentless node (the SourceFile) falls through to `return node`.
    let Some(parent) = node.parent() else {
        return node;
    };
    // /**/<modifier> [|name|] ...
    // /**/<modifier> <class|interface|type|enum|module|namespace|function|get|set> [|name|] ...
    // /**/<class|interface|type|enum|module|namespace|function|get|set> [|name|] ...
    // /**/import [|name|] = ...
    //
    // NOTE: If the node is a modifier, we don't adjust its location if it is the `default` modifier as that is handled
    // specially by `getSymbolAtLocation`.
    let is_modifier = |node: P<Node>| -> bool {
        if ast::is_modifier(node) && (for_rename || node.kind() != Kind::DefaultKeyword) {
            return ast::can_have_modifiers(parent) && parent.modifier_nodes().contains(&node);
        }
        match node.kind() {
            Kind::ClassKeyword => {
                ast::is_class_declaration(parent) || ast::is_class_expression(node)
            }
            Kind::FunctionKeyword => {
                ast::is_function_declaration(parent) || ast::is_function_expression(node)
            }
            Kind::InterfaceKeyword => ast::is_interface_declaration(parent),
            Kind::EnumKeyword => ast::is_enum_declaration(parent),
            Kind::TypeKeyword => ast::is_type_alias_declaration(parent),
            Kind::NamespaceKeyword | Kind::ModuleKeyword => ast::is_module_declaration(parent),
            Kind::ImportKeyword => ast::is_import_equals_declaration(parent),
            Kind::GetKeyword => ast::is_get_accessor_declaration(parent),
            Kind::SetKeyword => ast::is_set_accessor_declaration(parent),
            _ => false,
        }
    };
    if is_modifier(node) {
        if source_file.is_none() {
            source_file = ast::get_source_file_of_node(node);
        }
        if let Some(location) =
            get_adjusted_location_for_declaration(parent, for_rename, source_file.unwrap())
        {
            return location;
        }
    }

    // /**/<var|let|const> [|name|] ...
    if (node.kind() == Kind::VarKeyword
        || node.kind() == Kind::ConstKeyword
        || node.kind() == Kind::LetKeyword)
        && ast::is_variable_declaration_list(parent)
        && parent.as_variable_declaration_list().declarations.nodes().len() == 1
    {
        let declaration = parent.as_variable_declaration_list().declarations.nodes()[0];
        let name = declaration.name().unwrap();
        if ast::is_identifier(name) {
            return name;
        }
    }

    if node.kind() == Kind::TypeKeyword {
        // import /**/type [|name|] from ...;
        // import /**/type { [|name|] } from ...;
        // import /**/type { propertyName as [|name|] } from ...;
        // import /**/type ... from "[|module|]";
        if ast::is_import_clause(parent)
            && parent.is_type_only()
            && let Some(location) =
                get_adjusted_location_for_import_declaration(parent.parent().unwrap(), for_rename)
        {
            return location;
        }
        // export /**/type { [|name|] } from ...;
        // export /**/type { propertyName as [|name|] } from ...;
        // export /**/type * from "[|module|]";
        // export /**/type * as ... from "[|module|]";
        if ast::is_export_declaration(parent)
            && parent.is_type_only()
            && let Some(location) = get_adjusted_location_for_export_declaration(parent, for_rename)
        {
            return location;
        }
    }

    // import { propertyName /**/as [|name|] } ...
    // import * /**/as [|name|] ...
    // export { propertyName /**/as [|name|] } ...
    // export * /**/as [|name|] ...
    if node.kind() == Kind::AsKeyword {
        if parent.kind() == Kind::ImportSpecifier && parent.property_name().is_some()
            || parent.kind() == Kind::ExportSpecifier && parent.property_name().is_some()
            || parent.kind() == Kind::NamespaceImport
            || parent.kind() == Kind::NamespaceExport
        {
            return parent.name().unwrap();
        }
        if parent.kind() == Kind::ExportDeclaration
            && let Some(export_clause) = parent.as_export_declaration().export_clause
            && export_clause.kind() == Kind::NamespaceExport
        {
            return export_clause.name().unwrap();
        }
    }

    // /**/import [|name|] from ...;
    // /**/import { [|name|] } from ...;
    // /**/import { propertyName as [|name|] } from ...;
    // /**/import ... from "[|module|]";
    // /**/import "[|module|]";
    if node.kind() == Kind::ImportKeyword
        && parent.kind() == Kind::ImportDeclaration
        && let Some(location) = get_adjusted_location_for_import_declaration(parent, for_rename)
    {
        return location;
    }

    if node.kind() == Kind::ExportKeyword {
        // /**/export { [|name|] } ...;
        // /**/export { propertyName as [|name|] } ...;
        // /**/export * from "[|module|]";
        // /**/export * as ... from "[|module|]";
        if parent.kind() == Kind::ExportDeclaration
            && let Some(location) = get_adjusted_location_for_export_declaration(parent, for_rename)
        {
            return location;
        }
        // NOTE: We don't adjust the location of the `default` keyword as that is handled specially by `getSymbolAtLocation`.
        // /**/export default [|name|];
        // /**/export = [|name|];
        if parent.kind() == Kind::ExportAssignment {
            return ast::skip_outer_expressions(
                parent.expression().unwrap(),
                ast::OuterExpressionKinds::All,
            );
        }
    }
    // import name = /**/require("[|module|]");
    if node.kind() == Kind::RequireKeyword && parent.kind() == Kind::ExternalModuleReference {
        return parent.expression().unwrap();
    }
    // import ... /**/from "[|module|]";
    // export ... /**/from "[|module|]";
    if node.kind() == Kind::FromKeyword
        && (parent.kind() == Kind::ImportDeclaration || parent.kind() == Kind::ExportDeclaration)
        && let Some(module_specifier) = parent.module_specifier()
    {
        return module_specifier;
    }
    // class ... /**/extends [|name|] ...
    // class ... /**/implements [|name|] ...
    // class ... /**/implements name1, name2 ...
    // interface ... /**/extends [|name|] ...
    // interface ... /**/extends name1, name2 ...
    if (node.kind() == Kind::ExtendsKeyword || node.kind() == Kind::ImplementsKeyword)
        && parent.kind() == Kind::HeritageClause
        && parent.as_heritage_clause().token == node.kind()
    {
        let get_adjusted_location_for_heritage_clause = |node: P<Node>| -> Option<P<Node>> {
            // /**/extends [|name|]
            // /**/implements [|name|]
            let types = node.as_heritage_clause().types();
            if types.nodes().len() == 1 {
                return Some(ast::get_heritage_clause_element_name(types.nodes()[0]));
            }

            // fall through `getAdjustedLocation`
            //    /**/extends name1, name2 ...
            //    /**/implements name1, name2 ...
            None
        };

        if let Some(location) = get_adjusted_location_for_heritage_clause(parent) {
            return location;
        }
    }
    if node.kind() == Kind::ExtendsKeyword {
        // ... <T /**/extends [|U|]> ...
        if parent.kind() == Kind::TypeParameter
            && let Some(constraint) = parent.as_type_parameter_declaration().constraint
            && constraint.kind() == Kind::TypeReference
        {
            return constraint.as_type_reference_node().type_name;
        }
        // ... T /**/extends [|U|] ? ...
        if parent.kind() == Kind::ConditionalType {
            let extends_type = parent.as_conditional_type_node().extends_type;
            if extends_type.kind() == Kind::TypeReference {
                return extends_type.as_type_reference_node().type_name;
            }
        }
    }
    // ... T extends /**/infer [|U|] ? ...
    if node.kind() == Kind::InferKeyword && parent.kind() == Kind::InferType {
        return parent.as_infer_type_node().type_parameter.name().unwrap();
    }
    // { [ [|K|] /**/in keyof T]: ... }
    if node.kind() == Kind::InKeyword
        && parent.kind() == Kind::TypeParameter
        && parent.parent().unwrap().kind() == Kind::MappedType
    {
        return parent.name().unwrap();
    }
    // /**/keyof [|T|]
    if node.kind() == Kind::KeyOfKeyword
        && parent.kind() == Kind::TypeOperator
        && parent.as_type_operator_node().operator == Kind::KeyOfKeyword
        && let Some(parent_type) = parent.type_node()
        && parent_type.kind() == Kind::TypeReference
    {
        return parent_type.as_type_reference_node().type_name;
    }
    // /**/readonly [|name|][]
    if node.kind() == Kind::ReadonlyKeyword
        && parent.kind() == Kind::TypeOperator
        && parent.as_type_operator_node().operator == Kind::ReadonlyKeyword
        && let Some(parent_type) = parent.type_node()
        && parent_type.kind() == Kind::ArrayType
        && parent_type.as_array_type_node().element_type.kind() == Kind::TypeReference
    {
        return parent_type.as_array_type_node().element_type.as_type_reference_node().type_name;
    }

    if !for_rename {
        // /**/new [|name|]
        // /**/void [|name|]
        // /**/void obj.[|name|]
        // /**/typeof [|name|]
        // /**/typeof obj.[|name|]
        // /**/await [|name|]
        // /**/await obj.[|name|]
        // /**/yield [|name|]
        // /**/yield obj.[|name|]
        // /**/delete obj.[|name|]
        if (node.kind() == Kind::NewKeyword && parent.kind() == Kind::NewExpression
            || node.kind() == Kind::VoidKeyword && parent.kind() == Kind::VoidExpression
            || node.kind() == Kind::TypeOfKeyword && parent.kind() == Kind::TypeOfExpression
            || node.kind() == Kind::AwaitKeyword && parent.kind() == Kind::AwaitExpression
            || node.kind() == Kind::YieldKeyword && parent.kind() == Kind::YieldExpression
            || node.kind() == Kind::DeleteKeyword && parent.kind() == Kind::DeleteExpression)
            && let Some(expr) = parent.expression()
        {
            return ast::skip_outer_expressions(expr, ast::OuterExpressionKinds::All);
        }

        // left /**/in [|name|]
        // left /**/instanceof [|name|]
        if (node.kind() == Kind::InKeyword || node.kind() == Kind::InstanceOfKeyword)
            && parent.kind() == Kind::BinaryExpression
            && parent.as_binary_expression().operator_token == node
        {
            return ast::skip_outer_expressions(
                parent.as_binary_expression().right(),
                ast::OuterExpressionKinds::All,
            );
        }

        // left /**/as [|name|]
        if node.kind() == Kind::AsKeyword
            && parent.kind() == Kind::AsExpression
            && let Some(as_expr_type) = parent.type_node()
            && as_expr_type.kind() == Kind::TypeReference
        {
            return as_expr_type.as_type_reference_node().type_name;
        }

        // for (... /**/in [|name|])
        // for (... /**/of [|name|])
        if node.kind() == Kind::InKeyword && parent.kind() == Kind::ForInStatement
            || node.kind() == Kind::OfKeyword && parent.kind() == Kind::ForOfStatement
        {
            return ast::skip_outer_expressions(
                parent.expression().unwrap(),
                ast::OuterExpressionKinds::All,
            );
        }
    }

    node
}

// utilities.go:681
pub fn get_adjusted_location_for_declaration(
    node: P<Node>,
    for_rename: bool,
    source_file: P<SourceFile>,
) -> Option<P<Node>> {
    if let Some(name) = node.name() {
        return Some(name);
    }
    if for_rename {
        return None;
    }
    match node.kind() {
        Kind::ClassDeclaration | Kind::FunctionDeclaration => {
            // for class and function declarations, use the `default` modifier
            // when the declaration is unnamed.
            node.modifier_nodes().iter().copied().find(|_| node.kind() == Kind::DefaultKeyword)
        }
        Kind::ClassExpression => {
            // for class expressions, use the `class` keyword when the class is unnamed
            astnav::find_child_of_kind(node, Kind::ClassKeyword, source_file)
        }
        Kind::FunctionExpression => {
            // for function expressions, use the `function` keyword when the function is unnamed
            astnav::find_child_of_kind(node, Kind::FunctionKeyword, source_file)
        }
        Kind::Constructor => Some(node),
        _ => None,
    }
}

// utilities.go:705
pub fn get_adjusted_location_for_import_declaration(
    node: P<Node>,
    for_rename: bool,
) -> Option<P<Node>> {
    let decl = node.as_import_declaration();
    if let Some(import_clause) = decl.import_clause {
        if let Some(name) = import_clause.name() {
            if import_clause.as_import_clause().named_bindings.is_some() {
                // do not adjust if we have both a name and named bindings
                return None;
            }
            // /**/import [|name|] from ...;
            // import /**/type [|name|] from ...;
            return Some(name);
        }

        // /**/import { [|name|] } from ...;
        // /**/import { propertyName as [|name|] } from ...;
        // /**/import * as [|name|] from ...;
        // import /**/type { [|name|] } from ...;
        // import /**/type { propertyName as [|name|] } from ...;
        // import /**/type * as [|name|] from ...;
        if let Some(named_bindings) = import_clause.as_import_clause().named_bindings {
            match named_bindings.kind() {
                Kind::NamedImports => {
                    // do nothing if there is more than one binding
                    let elements = named_bindings.elements();
                    if elements.len() != 1 {
                        return None;
                    }
                    return elements[0].name();
                }
                Kind::NamespaceImport => return named_bindings.name(),
                _ => {}
            }
        }
    }
    if !for_rename {
        // /**/import "[|module|]";
        // /**/import ... from "[|module|]";
        // import /**/type ... from "[|module|]";
        return Some(decl.module_specifier);
    }
    None
}

// utilities.go:748
pub fn get_adjusted_location_for_export_declaration(
    node: P<Node>,
    for_rename: bool,
) -> Option<P<Node>> {
    let decl = node.as_export_declaration();
    if let Some(export_clause) = decl.export_clause {
        // /**/export { [|name|] } ...
        // /**/export { propertyName as [|name|] } ...
        // /**/export * as [|name|] ...
        // export /**/type { [|name|] } from ...
        // export /**/type { propertyName as [|name|] } from ...
        // export /**/type * as [|name|] ...
        match export_clause.kind() {
            Kind::NamedExports => {
                // do nothing if there is more than one binding
                let elements = export_clause.elements();
                if elements.len() != 1 {
                    return None;
                }
                return elements[0].name();
            }
            Kind::NamespaceExport => return export_clause.name(),
            _ => {}
        }
    }
    if !for_rename {
        // /**/export * from "[|module|]";
        // export /**/type * from "[|module|]";
        return decl.module_specifier;
    }
    None
}

// utilities.go:792
pub fn get_meaning_from_location(node: P<Node>) -> SemanticMeaning {
    // todo: check if this function needs to be changed for jsdoc updates
    let node = get_adjusted_location(
        ast::get_reparsed_node_for_node(node).unwrap(),
        false, /*forRename*/
        None,
    );
    let parent = node.parent();
    if ast::is_source_file(node) {
        return SemanticMeaning::Value;
    }
    let parent = parent.unwrap();
    if ast::node_kind_is(
        parent,
        &[
            Kind::ExportAssignment,
            Kind::ExportSpecifier,
            Kind::ExternalModuleReference,
            Kind::ImportSpecifier,
            Kind::ImportClause,
        ],
    ) || parent.kind() == Kind::ImportEqualsDeclaration && Some(node) == parent.name()
    {
        return SemanticMeaning::All;
    }
    if is_in_right_side_of_internal_import_equals_declaration(node) {
        //     import a = |b|; // Namespace
        //     import a = |b.c|; // Value, type, namespace
        //     import a = |b.c|.d; // Namespace
        let mut name = Some(node);
        if node.kind() != Kind::QualifiedName {
            let node_parent = node.parent().unwrap();
            name = if node_parent.kind() == Kind::QualifiedName
                && node_parent.as_qualified_name().right == node
            {
                Some(node_parent)
            } else {
                None
            };
        }
        if let Some(name) = name
            && name.parent().unwrap().kind() == Kind::ImportEqualsDeclaration
        {
            return SemanticMeaning::All;
        }
        return SemanticMeaning::Namespace;
    }
    if ast::is_declaration_name(node) {
        return get_meaning_from_declaration(parent);
    }
    if ast::is_entity_name(node) && ast::is_jsdoc_name_reference_context(node) {
        return SemanticMeaning::All;
    }
    if is_type_reference(node) {
        return SemanticMeaning::Type;
    }
    if is_namespace_reference(node) {
        return SemanticMeaning::Namespace;
    }
    if ast::is_type_parameter_declaration(parent) {
        return SemanticMeaning::Type;
    }
    if ast::is_literal_type_node(parent) {
        // This might be T["name"], which is actually referencing a property and not a type. So allow both meanings.
        return SemanticMeaning::Type | SemanticMeaning::Value;
    }
    SemanticMeaning::Value
}

// utilities.go:846
pub fn get_meaning_from_declaration(node: P<Node>) -> SemanticMeaning {
    match node.kind() {
        Kind::VariableDeclaration
        | Kind::Parameter
        | Kind::BindingElement
        | Kind::PropertyDeclaration
        | Kind::PropertySignature
        | Kind::PropertyAssignment
        | Kind::ShorthandPropertyAssignment
        | Kind::MethodDeclaration
        | Kind::MethodSignature
        | Kind::Constructor
        | Kind::GetAccessor
        | Kind::SetAccessor
        | Kind::FunctionDeclaration
        | Kind::FunctionExpression
        | Kind::ArrowFunction
        | Kind::CatchClause
        | Kind::JsxAttribute => SemanticMeaning::Value,

        Kind::TypeParameter
        | Kind::InterfaceDeclaration
        | Kind::TypeAliasDeclaration
        | Kind::JSTypeAliasDeclaration
        | Kind::TypeLiteral => SemanticMeaning::Type,

        Kind::EnumMember | Kind::ClassDeclaration => SemanticMeaning::Value | SemanticMeaning::Type,

        Kind::ModuleDeclaration => {
            if (ast::is_ambient_module(node))
                || (ast::get_module_instance_state(node) == ast::ModuleInstanceState::Instantiated)
            {
                SemanticMeaning::Namespace | SemanticMeaning::Value
            } else {
                SemanticMeaning::Namespace
            }
        }

        // An external module can be a Value
        Kind::SourceFile => SemanticMeaning::Namespace | SemanticMeaning::Value,

        _ => SemanticMeaning::All,
    }
}

// utilities.go:880
pub fn get_intersecting_meaning_from_declarations(
    node: Option<P<Node>>,
    symbol: P<Symbol>,
    default_meaning: SemanticMeaning,
) -> SemanticMeaning {
    let Some(node) = node else {
        return default_meaning;
    };

    let mut meaning = get_meaning_from_location(node);
    let declarations = symbol.declarations();
    if declarations.is_empty() {
        return meaning;
    }

    let mut last_iteration_meaning = meaning;

    // !!! TODO check if the port is correct and the for loop is needed
    let iteration = |mut m: SemanticMeaning| -> SemanticMeaning {
        for &declaration in declarations {
            let declaration_meaning = get_meaning_from_declaration(declaration);

            if declaration_meaning.intersects(m) {
                m |= declaration_meaning;
            }
        }
        m
    };
    meaning = iteration(meaning);

    while meaning != last_iteration_meaning {
        // The result is order-sensitive, for instance if initialMeaning == Namespace, and declarations = [class, instantiated module]
        // we need to consider both as the initialMeaning intersects with the module in the namespace space, and the module
        // intersects with the class in the value space.
        // To achieve that we will keep iterating until the result stabilizes.

        // Remember the last meaning
        last_iteration_meaning = meaning;
        meaning = iteration(meaning);
    }

    meaning
}

// Returns the node in an `extends` or `implements` clause of a class or interface.
// utilities.go:922
pub fn get_all_super_type_nodes(node: P<Node>) -> Vec<P<Node>> {
    if ast::is_interface_declaration(node) {
        return ast::get_heritage_elements(node, Kind::ExtendsKeyword).to_vec();
    }
    if ast::is_class_like(node) {
        let mut result: Vec<P<Node>> =
            ast::get_class_extends_heritage_element(node).into_iter().collect();
        result.extend_from_slice(ast::get_implements_heritage_clause_elements(node));
        return result;
    }
    Vec::new()
}

// utilities.go:935
pub fn get_parent_symbols_of_property_access(
    location: P<Node>,
    symbol: P<Symbol>,
    ch: &mut Checker,
) -> Vec<P<Symbol>> {
    if !is_right_side_of_property_access(location) {
        return Vec::new();
    }
    let lhs_type = ch.get_type_at_location(location.parent().unwrap().expression().unwrap());
    let possible_symbols: Vec<P<Type>> =
        if lhs_type.flags().intersects(TypeFlags::UnionOrIntersection) {
            lhs_type.types().to_vec()
        } else if lhs_type.symbol() != symbol.parent() {
            vec![lhs_type]
        } else {
            Vec::new()
        };
    possible_symbols
        .into_iter()
        .filter_map(|t| match t.symbol() {
            Some(s) if s.flags().intersects(SymbolFlags::Class | SymbolFlags::Interface) => Some(s),
            _ => None,
        })
        .collect()
}

// Find symbol of the given property-name and add the symbol to the given result array
// @param symbol a symbol to start searching for the given propertyName
// @param propertyName a name of property to search for
// @param cb a cache of symbol from previous iterations of calling this function to prevent infinite revisiting of the same symbol.
//
//	The value of previousIterationSymbol is undefined when the function is first called.
// utilities.go:962
pub fn get_property_symbols_from_base_types(
    symbol: P<Symbol>,
    property_name: &str,
    checker: &mut Checker,
    cb: &mut dyn FnMut(&mut Checker, P<Symbol>) -> Option<P<Symbol>>,
) -> Option<P<Symbol>> {
    fn recur(
        symbol: P<Symbol>,
        property_name: &str,
        checker: &mut Checker,
        cb: &mut dyn FnMut(&mut Checker, P<Symbol>) -> Option<P<Symbol>>,
        seen: &mut FxHashSet<P<Symbol>>,
    ) -> Option<P<Symbol>> {
        // Use `addToSeen` to ensure we don't infinitely recurse in this situation:
        //      interface C extends C {
        //          /*findRef*/propName: string;
        //      }
        if !symbol.flags().intersects(SymbolFlags::Class | SymbolFlags::Interface)
            || !seen.insert(symbol)
        {
            return None;
        }
        for &declaration in symbol.declarations() {
            for type_reference in get_all_super_type_nodes(declaration) {
                let property_type = checker.get_type_at_location(type_reference);
                if let Some(property_type_symbol) = property_type.symbol() {
                    // Visit the typeReference as well to see if it directly or indirectly uses that property
                    if let Some(property_symbol) =
                        checker.get_property_of_type_exported(property_type, property_name)
                        && let Some(result) = checker
                            .get_root_symbols(property_symbol)
                            .into_iter()
                            .find_map(|root_symbol| {
                                if let Some(result) = cb(checker, root_symbol) {
                                    return Some(Some(result));
                                }

                                None
                            })
                    {
                        return result;
                    }
                    if let Some(result) =
                        recur(property_type_symbol, property_name, checker, cb, seen)
                    {
                        return Some(result);
                    }
                }
            }
        }
        None
    }
    let mut seen = FxHashSet::default();
    recur(symbol, property_name, checker, cb, &mut seen)
}

// utilities.go:995
pub fn get_property_symbol_from_binding_element(
    checker: &mut Checker,
    binding_element: P<Node>,
) -> Option<P<Symbol>> {
    let type_of_pattern = checker.get_type_at_location(binding_element.parent().unwrap());
    checker.get_property_of_type_exported(type_of_pattern, binding_element.name().unwrap().text())
}

// utilities.go:1002
pub fn get_property_symbol_of_object_binding_pattern_without_property_name(
    symbol: P<Symbol>,
    checker: &mut Checker,
) -> Option<P<Symbol>> {
    let binding_element = ast::get_declaration_of_kind(symbol, Kind::BindingElement);
    if let Some(binding_element) = binding_element
        && is_object_binding_element_without_property_name(binding_element)
    {
        return get_property_symbol_from_binding_element(checker, binding_element);
    }
    None
}

// utilities.go:1010
pub fn get_target_label(reference_node: Option<P<Node>>, label_name: &str) -> Option<P<Node>> {
    // todo: rewrite as `ast.FindAncestor`
    if let Some(result) = std::iter::successors(reference_node, |n| n.parent()).find_map(|n| {
        if n.kind() == Kind::LabeledStatement && n.label().unwrap().text() == label_name {
            Some(n.label())
        } else {
            None
        }
    }) {
        return result;
    }
    None
}

// utilities.go:1021
pub fn skip_constraint(t: P<Type>, type_checker: &mut Checker) -> P<Type> {
    if t.is_type_parameter()
        && let Some(c) = type_checker.get_base_constraint_of_type_exported(t)
    {
        return c;
    }
    t
}

// utilities.go:1031
#[derive(Default)]
pub struct CaseClauseTrackerState {
    existing_strings: FxHashSet<String>,
    // jsnum.Number keys compared by f64 value, like Go float64 map keys (NaN never matches, -0 == 0)
    existing_numbers: Vec<jsnum::Number>,
    existing_big_ints: FxHashSet<PseudoBigInt>,
}

// utilities.go:1037-1047: `trackerAddValue` = string | jsnum.Number, `trackerHasValue` = string | jsnum.Number | jsnum.PseudoBigInt
pub enum TrackerValue {
    String(String),
    Number(jsnum::Number),
    BigInt(PseudoBigInt),
}

// utilities.go:1043
pub trait CaseClauseTracker {
    fn add_value(&mut self, value: TrackerValue);
    fn has_value(&self, value: &TrackerValue) -> bool;
}

impl CaseClauseTracker for CaseClauseTrackerState {
    // utilities.go:1048
    #[expect(
        clippy::float_cmp,
        reason = "Exact equality preserves JavaScript numeric identity; an epsilon comparison would merge distinct values."
    )]
    fn add_value(&mut self, value: TrackerValue) {
        match value {
            TrackerValue::String(v) => {
                self.existing_strings.insert(v);
            }
            TrackerValue::Number(v) => {
                if !self.existing_numbers.iter().any(|n| n.0 == v.0) {
                    self.existing_numbers.push(v);
                }
            }
            TrackerValue::BigInt(_) => panic!("Unsupported type: jsnum.PseudoBigInt"),
        }
    }

    // utilities.go:1059
    #[expect(
        clippy::float_cmp,
        reason = "Exact equality preserves JavaScript numeric identity; an epsilon comparison would merge distinct values."
    )]
    fn has_value(&self, value: &TrackerValue) -> bool {
        match value {
            TrackerValue::String(v) => self.existing_strings.contains(v),
            TrackerValue::Number(v) => self.existing_numbers.iter().any(|n| n.0 == v.0),
            TrackerValue::BigInt(v) => self.existing_big_ints.contains(v),
        }
    }
}

// utilities.go:1072
pub fn new_case_clause_tracker(
    type_checker: &mut Checker,
    clauses: &[P<Node>],
) -> Box<dyn CaseClauseTracker> {
    let mut c = CaseClauseTrackerState::default();
    for &clause in clauses {
        if !ast::is_default_clause(clause) {
            let expression = ast::skip_parentheses(clause.expression().unwrap());
            if ast::is_literal_expression(expression) {
                match expression.kind() {
                    Kind::NoSubstitutionTemplateLiteral | Kind::StringLiteral => {
                        c.existing_strings.insert(expression.text().to_string());
                    }
                    Kind::NumericLiteral => {
                        c.add_value(TrackerValue::Number(jsnum::from_string(expression.text())));
                    }
                    Kind::BigIntLiteral => {
                        c.existing_big_ints.insert(jsnum::parse_valid_big_int(expression.text()));
                    }
                    _ => {}
                }
            } else {
                let symbol =
                    type_checker.get_symbol_at_location_exported(clause.expression().unwrap());
                if let Some(symbol) = symbol
                    && let Some(value_declaration) = symbol.value_declaration()
                    && ast::is_enum_member(value_declaration)
                {
                    let enum_value = type_checker.get_constant_value(value_declaration);
                    match enum_value {
                        Some(LiteralValue::String(s)) => {
                            c.add_value(TrackerValue::String(s.to_string()));
                        }
                        Some(LiteralValue::Number(n)) => {
                            c.add_value(TrackerValue::Number(n));
                        }
                        Some(LiteralValue::Boolean(_)) => panic!("Unsupported type: bool"),
                        Some(LiteralValue::BigInt(_)) => {
                            panic!("Unsupported type: jsnum.PseudoBigInt")
                        }
                        None => {}
                    }
                }
            }
        }
    }
    Box::new(c)
}

// utilities.go:1105
pub fn range_contains_range(r1: TextRange, r2: TextRange) -> bool {
    start_end_contains_range(r1.pos(), r1.end(), r2)
}

// utilities.go:1109
pub fn start_end_contains_range(start: TextPos, end: TextPos, text_range: TextRange) -> bool {
    start <= text_range.pos() && end >= text_range.end()
}

// utilities.go:1113
pub fn get_possible_generic_signatures(
    called: P<Node>,
    type_argument_count: usize,
    c: &mut Checker,
) -> Vec<P<Signature>> {
    let mut type_at_location = c.get_type_at_location(called);
    let called_parent = called.parent().unwrap();
    if ast::is_optional_chain(called_parent) {
        type_at_location = remove_optionality(
            type_at_location,
            ast::is_optional_chain_root(called_parent),
            true, /*isOptionalChain*/
            c,
        );
    }
    let signatures = if ast::is_new_expression(called_parent) {
        c.get_signatures_of_type_exported(type_at_location, SignatureKind::Construct)
    } else {
        c.get_signatures_of_type_exported(type_at_location, SignatureKind::Call)
    };
    signatures
        .iter()
        .copied()
        .filter(|s| {
            !s.type_parameters().is_empty() && s.type_parameters().len() >= type_argument_count
        })
        .collect()
}

// utilities.go:1129
pub fn remove_optionality(
    t: P<Type>,
    is_optional_expression: bool,
    is_optional_chain: bool,
    c: &mut Checker,
) -> P<Type> {
    if is_optional_expression {
        return c.get_non_nullable_type(t);
    } else if is_optional_chain {
        return c.get_non_optional_type(t);
    }
    t
}

// utilities.go:1138
pub fn is_no_substitution_template_literal(node: P<Node>) -> bool {
    node.kind() == Kind::NoSubstitutionTemplateLiteral
}

// utilities.go:1142
pub fn is_tagged_template_expression(node: P<Node>) -> bool {
    node.kind() == Kind::TaggedTemplateExpression
}

// utilities.go:1146
pub fn is_inside_template_literal(
    node: P<Node>,
    position: u32,
    source_file: P<SourceFile>,
) -> bool {
    let end = node.end().expect_source("parsed template literals have source positions");
    ast::is_template_literal_kind(node.kind())
        && (scanner::get_token_pos_of_node(node, source_file, false) < position && position < end
            || (ast::is_unterminated_literal(node) && position == end))
}

// Pseudo-literals
// utilities.go:1151
pub fn is_template_head(node: P<Node>) -> bool {
    node.kind() == Kind::TemplateHead
}

// utilities.go:1155
pub fn is_template_tail(node: P<Node>) -> bool {
    node.kind() == Kind::TemplateTail
}

// utilities.go:1159
pub fn find_preceding_matching_token(
    token: P<Node>,
    matching_token_kind: Kind,
    source_file: P<SourceFile>,
) -> Option<P<Node>> {
    let close_token_text = scanner::token_to_string(token.kind());
    let matching_token_text = scanner::token_to_string(matching_token_kind);
    // Text-scan based fast path - can be bamboozled by comments and other trivia, but often provides
    // a good, fast approximation without too much extra work in the cases where it fails.
    let text = source_file.text();
    let Some(best_guess_index) = text.rfind(matching_token_text) else {
        return None; // if the token text doesn't appear in the file, there can't be a match - super fast bail
    };
    // we can only use the textual result directly if we didn't have to count any close tokens within the range
    let close_index = text.rfind(close_token_text);
    if close_index.is_none_or(|close_index| close_index < best_guess_index) {
        let node_at_guess = astnav::find_preceding_token(
            source_file,
            u32::try_from(best_guess_index + 1).expect("source position exceeds u32"),
        );
        if let Some(node_at_guess) = node_at_guess
            && node_at_guess.kind() == matching_token_kind
        {
            return Some(node_at_guess);
        }
    }
    let token_kind = token.kind();
    let mut remaining_matching_tokens = 0;
    let mut token = token;
    loop {
        let preceding = astnav::find_preceding_token(
            source_file,
            token.pos().expect_source("parsed tokens have source positions"),
        )?;
        token = preceding;
        if token.kind() == matching_token_kind {
            if remaining_matching_tokens == 0 {
                return Some(token);
            }
            remaining_matching_tokens -= 1;
        } else if token.kind() == token_kind {
            remaining_matching_tokens += 1;
        }
    }
}

// utilities.go:1193
pub fn find_containing_list(node: P<Node>, file: P<SourceFile>) -> Option<P<NodeList>> {
    // The node might be a list element (nonsynthetic) or a comma (synthetic). Either way, it will
    // be parented by the container of the SyntaxList, not the SyntaxList itself.
    let list: std::rc::Rc<std::cell::Cell<Option<P<NodeList>>>> = std::rc::Rc::default();
    let visit_node: astnav::VisitNodeFn =
        std::rc::Rc::new(|n: Option<P<Node>>, _visitor: &mut ast::NodeVisitor| n);
    let list_ref = Rc::clone(&list);
    let node_loc = node.loc();
    let visit_nodes: astnav::VisitNodesFn =
        std::rc::Rc::new(move |nodes: Option<P<NodeList>>, _visitor: &mut ast::NodeVisitor| {
            if let Some(n) = nodes
                && range_contains_range(n.loc.get(), node_loc)
            {
                list_ref.set(Some(n));
            }
            nodes
        });
    astnav::visit_each_child_and_jsdoc(
        node.parent().unwrap(),
        file,
        Some(visit_node),
        Some(visit_nodes),
    );
    list.get()
}

// utilities.go:1210
pub fn get_leading_comment_ranges_of_node(
    node: P<Node>,
    file: P<SourceFile>,
) -> Option<scanner::CommentRangeIter> {
    if node.kind() == Kind::JsxText {
        return None;
    }
    Some(scanner::get_leading_comment_ranges(
        file.text(),
        node.pos().expect_source("parsed nodes have source positions"),
    ))
}

// Equivalent to Strada's `node.getChildren()` for non-JSDoc nodes.
// utilities.go:1218
pub fn get_children_from_non_jsdoc_node(node: P<Node>, source_file: P<SourceFile>) -> Vec<P<Node>> {
    let mut child_nodes: Vec<P<Node>> = Vec::new();
    node.for_each_child(&mut |child| {
        child_nodes.push(child);
        false
    });

    // If the node has no children, don't scan for tokens.
    // This prevents creating tokens for leaf nodes' own text.
    if child_nodes.is_empty() {
        return Vec::new();
    }

    let mut children: Vec<P<Node>> = Vec::new();
    let mut pos = node.pos().expect_source("parsed nodes have source positions");
    for child in child_nodes {
        let mut scanner = scanner::get_scanner_for_source_file(source_file, pos);
        while pos < child.pos().expect_source("parsed nodes have source positions") {
            let token = scanner.token();
            let token_full_start = scanner.token_full_start();
            let token_end = scanner.token_end();
            children.push(source_file.get_or_create_token(
                token,
                token_full_start,
                token_end,
                node,
                scanner.token_flags(),
            ));
            pos = token_end;
            scanner.scan();
        }
        children.push(child);
        pos = child.end().expect_source("parsed nodes have source positions");
    }
    let mut scanner = scanner::get_scanner_for_source_file(source_file, pos);
    while pos < node.end().expect_source("parsed nodes have source positions") {
        let token = scanner.token();
        let token_full_start = scanner.token_full_start();
        let token_end = scanner.token_end();
        children.push(source_file.get_or_create_token(
            token,
            token_full_start,
            token_end,
            node,
            scanner.token_flags(),
        ));
        pos = token_end;
        scanner.scan();
    }
    children
}

// Returns the containing object literal property declaration given a possible name node, e.g. "a" in x = { "a": 1 }
// utilities.go:1258
pub fn get_containing_object_literal_element(node: P<Node>) -> Option<P<Node>> {
    let element = get_containing_object_literal_element_worker(node);
    if let Some(element) = element {
        let parent = element.parent().unwrap();
        if ast::is_object_literal_expression(parent) || ast::is_jsx_attributes(parent) {
            return Some(element);
        }
    }
    None
}

// utilities.go:1266
pub fn get_containing_object_literal_element_worker(node: P<Node>) -> Option<P<Node>> {
    let mut fallthrough = false;
    match node.kind() {
        Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral | Kind::NumericLiteral => {
            let parent = node.parent().unwrap();
            if parent.kind() == Kind::ComputedPropertyName {
                if is_object_literal_or_jsx_element(parent.parent().unwrap()) {
                    return parent.parent();
                }
                return None;
            }
            fallthrough = true;
        }
        Kind::Identifier | Kind::JsxNamespacedName => fallthrough = true,
        _ => {}
    }
    if fallthrough {
        let parent = node.parent().unwrap();
        if is_object_literal_or_jsx_element(parent)
            && (parent.parent().unwrap().kind() == Kind::ObjectLiteralExpression
                || parent.parent().unwrap().kind() == Kind::JsxAttributes)
            && parent.name() == Some(node)
        {
            return Some(parent);
        }
    }
    None
}

// utilities.go:1284
pub fn is_object_literal_or_jsx_element(node: P<Node>) -> bool {
    ast::is_object_literal_element(node)
        || ast::is_jsx_attribute(node)
        || ast::is_jsx_spread_attribute(node)
}

// FindAllReferences.toContextSpan
// utilities.go:1297
pub fn to_context_range(
    text_range: Option<TextRange>,
    context_file: P<SourceFile>,
    context: Option<P<Node>>,
) -> Option<TextRange> {
    let Some(context) = context else {
        return text_range;
    };
    let text_range = text_range.expect("nil textRange");
    // !!! isContextWithStartAndEndNode
    let context_range = get_range_of_node(context, Some(context_file), None /*endNode*/);
    if context_range.pos() != text_range.pos() || context_range.end() != text_range.end() {
        return Some(context_range);
    }
    None
}

// utilities.go:1310
pub fn get_reference_at_position(
    source_file: P<SourceFile>,
    position: u32,
    program: &Program,
) -> Option<RefInfo> {
    if let Some(reference_path) =
        find_reference_in_position(source_file.referenced_files(), position)
    {
        if let Some(file) = program.get_source_file_from_reference(source_file, reference_path) {
            return Some(RefInfo {
                reference: Some(reference_path),
                file_name: file.file_name().to_string(),
                file: Some(file),
            });
        }
        return None;
    }

    if let Some(type_reference_directive) =
        find_reference_in_position(source_file.type_reference_directives(), position)
    {
        if let Some(reference) = program
            .get_resolved_type_reference_directive_from_type_reference_directive(
                type_reference_directive,
                source_file,
            )
            && let Some(file) = program.get_source_file(reference.resolved_file_name)
        {
            return Some(RefInfo {
                reference: Some(type_reference_directive),
                file_name: file.file_name().to_string(),
                file: Some(file),
            });
        }
        return None;
    }

    if let Some(lib_reference_directive) =
        find_reference_in_position(source_file.lib_reference_directives(), position)
    {
        if let Some(file) = program.get_lib_file_from_reference(lib_reference_directive) {
            return Some(RefInfo {
                reference: Some(lib_reference_directive),
                file_name: file.file_name().to_string(),
                file: Some(file),
            });
        }
        return None;
    }

    if source_file.imports().is_empty() && source_file.module_augmentations().is_empty() {
        return None;
    }

    let node = astnav::get_touching_token(source_file, position);
    if !is_module_specifier_like(node) || !tspath::is_external_module_name_relative(node.text()) {
        return None;
    }

    if let Some(resolution) = program.get_resolved_module_from_module_specifier(source_file, node) {
        let mut file_name = resolution.resolved_file_name.to_string();
        if file_name.is_empty() {
            file_name = tspath::resolve_path(
                &tspath::get_directory_path(source_file.file_name()),
                &[node.text()],
            );
        }
        return Some(RefInfo {
            file: program.get_source_file(&file_name),
            file_name,
            reference: None,
        });
    }

    None
}

// utilities.go:1357
pub fn get_contextual_type_from_parent(
    node: P<Node>,
    type_checker: &mut Checker,
    context_flags: ContextFlags,
) -> Option<P<Type>> {
    let parent = ast::walk_up_parenthesized_expressions(node.parent().unwrap()).unwrap();
    match parent.kind() {
        Kind::NewExpression => type_checker.get_contextual_type_exported(parent, context_flags),
        Kind::BinaryExpression => {
            let bin = parent.as_binary_expression();
            if is_equality_operator_kind(bin.operator_token.kind()) {
                return Some(type_checker.get_type_at_location(if node == bin.right() {
                    bin.left
                } else {
                    bin.right()
                }));
            }
            type_checker.get_contextual_type_exported(node, context_flags)
        }
        Kind::CaseClause => Some(get_switched_type(parent, type_checker)),
        _ => type_checker.get_contextual_type_exported(node, context_flags),
    }
}

// utilities.go:1377
pub fn get_contextual_type_from_parent_or_ancestor_type_node(
    node: P<Node>,
    type_checker: &mut Checker,
) -> Option<P<Type>> {
    if node.flags().intersects(NodeFlags::JSDoc)
        && !node.flags().intersects(NodeFlags::JavaScriptFile)
    {
        return None;
    }

    let contextual_type = get_contextual_type_from_parent(node, type_checker, ContextFlags::None);
    if contextual_type.is_some() {
        return contextual_type;
    }

    if let Some(ancestor_type_node) = get_ancestor_type_node(node) {
        return Some(type_checker.get_type_at_location(ancestor_type_node));
    }

    None
}

// utilities.go:1394
pub fn get_ancestor_type_node(node: P<Node>) -> Option<P<Node>> {
    let mut last_type_node: Option<P<Node>> = None;
    ast::find_ancestor(node, |n| {
        if ast::is_type_node(n) {
            last_type_node = Some(n);
        }
        let parent = n.parent().unwrap();
        !ast::is_qualified_name(parent)
            && !ast::is_type_node(parent)
            && !ast::is_type_element(parent)
    });
    last_type_node
}

// utilities.go:1406
pub fn is_source_file_with_global_exports(node: Option<P<Node>>) -> bool {
    match node {
        Some(node) => ast::is_source_file(node) && node.as_source_file().global_exports().is_some(),
        None => false,
    }
}
