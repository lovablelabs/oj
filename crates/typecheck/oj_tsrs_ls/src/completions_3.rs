// completions.go, lines 3468-5487.

use std::sync::{Mutex, OnceLock};

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{
    self as ast, FindAncestorResult, Kind, ModifierFlags, NameTableEntry, Node, SourceFile, Symbol,
    SymbolFlags,
};
use tsrs_checker::{self as checker, Checker, ContextFlags, Type, TypeFlags};
use tsrs_core::context::Context;
use tsrs_core::stringutil::{self, Comparison};
use tsrs_core::{LanguageVariant, P};
use tsrs_lsproto as lsproto;
use tsrs_scanner as scanner;

use crate::astnav;
use crate::completions::*;
use crate::languageservice::LanguageService;
use crate::lsutil::{self, ScriptElementKind, ScriptElementKindModifier, UserPreferences};
use crate::signaturehelp::get_immediately_containing_argument_info;
use crate::utilities::{position_belongs_to_node, quote};

// completions.go:3468
pub(crate) fn get_contextual_type(
    previous_token: P<Node>,
    position: u32,
    file: P<SourceFile>,
    type_checker: &mut Checker,
) -> Option<P<Type>> {
    let parent = previous_token.parent().unwrap();
    match previous_token.kind() {
        Kind::Identifier => return crate::utilities::get_contextual_type_from_parent(previous_token, type_checker, ContextFlags::None),
        Kind::EqualsToken => {
            return match parent.kind() {
                Kind::VariableDeclaration => type_checker.get_contextual_type_exported(parent.initializer().unwrap(), ContextFlags::None),
                Kind::BinaryExpression => Some(type_checker.get_type_at_location(parent.as_binary_expression().left)),
                Kind::JsxAttribute => type_checker.get_contextual_type_for_jsx_attribute_exported(parent),
                _ => None,
            };
        }
        Kind::NewKeyword => return type_checker.get_contextual_type_exported(parent, ContextFlags::None),
        Kind::CaseKeyword => {
            let case_clause = if ast::is_case_clause(parent) { Some(parent) } else { None };
            if let Some(case_clause) = case_clause {
                return Some(get_switched_type(case_clause, type_checker));
            }
            return None;
        }
        Kind::OpenBraceToken => {
            if ast::is_jsx_expression(parent) && !ast::is_jsx_element(parent.parent().unwrap()) && !ast::is_jsx_fragment(parent.parent().unwrap()) {
                return type_checker.get_contextual_type_for_jsx_attribute_exported(parent.parent().unwrap());
            }
            return None;
        }
        Kind::OpenBracketToken => {
            // When completing after `[` in an array literal (e.g., `[/*here*/]`),
            // we should provide contextual type for the first element
            if ast::is_array_literal_expression(parent) {
                let contextual_array_type = type_checker.get_contextual_type_exported(parent, ContextFlags::None);
                if contextual_array_type.is_some() {
                    // Get the type for the first element (index 0)
                    return type_checker.get_contextual_type_for_array_literal_at_position(contextual_array_type, parent, position);
                }
            }
            return None;
        }
        Kind::CloseBracketToken => {
            // When completing after `]` (e.g., `[x]/*here*/`), we should not provide a contextual type
            // for the closing bracket token itself. Without this case, CloseBracketToken would fall through
            // to the default case, and if the parent is an array literal, GetContextualType would try to
            // find the token's index in the array elements (returning -1), leading to an out-of-bounds panic
            // in getContextualTypeForElementExpression.
            return None;
        }
        Kind::QuestionToken => {
            // When completing after `?` in a ternary conditional (e.g., `foo(a ? /*here*/)`),
            // we need to look at the parent conditional expression to find the contextual type.
            if ast::is_conditional_expression(parent) {
                return get_contextual_type_for_conditional_expression(parent, position, file, type_checker);
            }
            return None;
        }
        Kind::ColonToken => {
            // When completing after `:` in a ternary conditional (e.g., `foo(a ? b : /*here*/)`),
            // we need to look at the parent conditional expression to find the contextual type.
            // Only handle this if parent is ConditionalExpression, otherwise fall through to default
            // (colons are used in other contexts like object literals, type annotations, etc.)
            if ast::is_conditional_expression(parent) {
                return get_contextual_type_for_conditional_expression(parent, position, file, type_checker);
            }
        }
        Kind::CommaToken
            // When completing after `,` in an array literal (e.g., `[x, /*here*/]`),
            // we should provide contextual type for the element after the comma.
            if ast::is_array_literal_expression(parent) => {
                let contextual_array_type = type_checker.get_contextual_type_exported(parent, ContextFlags::None);
                if contextual_array_type.is_some() {
                    return type_checker.get_contextual_type_for_array_literal_at_position(contextual_array_type, parent, position);
                }
                return None;
            }
        _ => {}
    }
    // Default case: see if we're in an argument position.
    let arg_info = get_argument_info_for_completions(previous_token, position, file, type_checker);
    if let Some(arg_info) = arg_info {
        Some(type_checker.get_contextual_type_for_argument_at_index_exported(
            arg_info.invocation,
            arg_info.argument_index,
        ))
    } else if is_equality_operator_kind(previous_token.kind())
        && ast::is_binary_expression(parent)
        && is_equality_operator_kind(parent.as_binary_expression().operator_token.kind())
    {
        // completion at `x ===/**/`
        Some(type_checker.get_type_at_location(parent.as_binary_expression().left))
    } else {
        let contextual_type = type_checker
            .get_contextual_type_exported(previous_token, ContextFlags::IgnoreNodeInferences);
        if contextual_type.is_some() {
            return contextual_type;
        }
        type_checker.get_contextual_type_exported(previous_token, ContextFlags::None)
    }
}

// completions.go:3557
pub(crate) fn get_switched_type(case_clause: P<Node>, type_checker: &mut Checker) -> P<Type> {
    type_checker
        .get_type_at_location(case_clause.parent().unwrap().parent().unwrap().expression().unwrap())
}

// completions.go:3561
pub(crate) fn is_equality_operator_kind(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::EqualsEqualsEqualsToken
            | Kind::EqualsEqualsToken
            | Kind::ExclamationEqualsEqualsToken
            | Kind::ExclamationEqualsToken
    )
}

// We disregard boolean literals for completion purposes.
// completions.go:3572
pub(crate) fn is_literal(t: P<Type>) -> bool {
    t.is_string_literal() || t.is_number_literal() || t.is_big_int_literal()
}

// completions.go:3576
pub(crate) fn get_recommended_completion(
    previous_token: P<Node>,
    contextual_type: P<Type>,
    type_checker: &mut Checker,
) -> Option<P<Symbol>> {
    let types: Vec<P<Type>> = if contextual_type.is_union() {
        contextual_type.types().to_vec()
    } else {
        vec![contextual_type]
    };
    // For a union, return the first one with a recommended completion.
    if let Some(result) = types.into_iter().find_map(|t| {
        let symbol = t.symbol();
        // Don't make a recommended completion for an abstract class.
        if let Some(symbol) = symbol
            && symbol
                .flags()
                .intersects(SymbolFlags::EnumMember | SymbolFlags::Enum | SymbolFlags::Class)
            && !is_abstract_constructor_symbol(symbol)
        {
            let result = get_first_symbol_in_chain(symbol, Some(previous_token), type_checker);
            if result.is_some() {
                return Some(result);
            }
        }

        None
    }) {
        return result;
    }
    None
}

// completions.go:3599
fn is_abstract_constructor_symbol(symbol: P<Symbol>) -> bool {
    if symbol.flags().intersects(SymbolFlags::Class) {
        let declaration = ast::get_class_like_declaration_of_symbol(symbol);
        return declaration
            .is_some_and(|d| ast::has_syntactic_modifier(d, ModifierFlags::Abstract));
    }
    false
}

// completions.go:3607
pub(crate) fn starts_with_quote(s: &str) -> bool {
    let r = s.chars().next().unwrap_or('\u{FFFD}');
    r == '"' || r == '\''
}

// completions.go:3612
pub(crate) fn get_closest_symbol_declaration(
    context_token: Option<P<Node>>,
    location: P<Node>,
) -> Option<P<Node>> {
    let context_token = context_token?;

    let mut closest_declaration = ast::find_ancestor_or_quit(context_token, |node| {
        if ast::is_function_block(node)
            || is_arrow_function_body(node)
            || ast::is_binding_pattern(node)
        {
            return FindAncestorResult::Quit;
        }

        if (ast::is_parameter_declaration(node) || ast::is_type_parameter_declaration(node))
            && !ast::is_index_signature_declaration(node.parent().unwrap())
        {
            return FindAncestorResult::True;
        }
        FindAncestorResult::False
    });

    if closest_declaration.is_none() {
        closest_declaration = ast::find_ancestor_or_quit(location, |node| {
            if ast::is_function_block(node)
                || is_arrow_function_body(node)
                || ast::is_binding_pattern(node)
            {
                return FindAncestorResult::Quit;
            }

            if ast::is_variable_declaration(node) {
                return FindAncestorResult::True;
            }
            FindAncestorResult::False
        });
    }
    closest_declaration
}

// completions.go:3644
fn is_arrow_function_body(node: P<Node>) -> bool {
    node.parent().is_some_and(|parent| {
        ast::is_arrow_function(parent)
            && (parent.body() == Some(node) ||
                // const a = () => /**/;
                node.kind() == Kind::EqualsGreaterThanToken)
    })
}

// completions.go:3651
pub(crate) fn is_in_type_parameter_default(context_token: Option<P<Node>>) -> bool {
    let Some(context_token) = context_token else {
        return false;
    };

    let mut node = context_token;
    let mut parent = context_token.parent();
    while let Some(p) = parent {
        if ast::is_type_parameter_declaration(p) {
            return p.as_type_parameter_declaration().default_type == Some(node)
                || node.kind() == Kind::EqualsToken;
        }
        node = p;
        parent = p.parent();
    }

    false
}

// completions.go:3669
pub(crate) fn is_deprecated(symbol: P<Symbol>, type_checker: &mut Checker) -> bool {
    let declarations = checker::skip_alias(symbol, type_checker).declarations();
    !declarations.is_empty()
        && declarations.iter().all(|&decl| type_checker.is_deprecated_declaration(decl))
}

impl LanguageService {
    // completions.go:3674
    pub(crate) fn get_replacement_range_for_context_token(
        &self,
        file: P<SourceFile>,
        context_token: Option<P<Node>>,
        position: u32,
    ) -> Option<lsproto::Range> {
        let context_token = context_token?;

        // !!! ensure range is single line
        match context_token.kind() {
            Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral => {
                self.create_range_from_string_literal_like_content(file, context_token, position)
            }
            _ => {
                let (lsp_range, fidelity) = self.create_lsp_range_from_node(context_token, file);
                if !fidelity.is_exact() {
                    return None;
                }
                Some(lsp_range)
            }
        }
    }

    // completions.go:3692
    pub(crate) fn create_range_from_string_literal_like_content(
        &self,
        file: P<SourceFile>,
        node: P<Node>,
        position: u32,
    ) -> Option<lsproto::Range> {
        let mut replacement_end = node
            .end()
            .expect_source("parsed literals have source positions")
            .checked_sub(1)
            .expect("literal has an opening quote");
        let node_start = astnav::get_start_of_node(node, file, false /*includeJSDoc*/);
        if ast::is_unterminated_literal(node) {
            // we return no replacement range only if unterminated string is empty
            if node_start == replacement_end {
                return None;
            }
            replacement_end =
                position.min(node.end().expect_source("parsed literals have source positions"));
        }
        let (lsp_range, fidelity) =
            self.create_lsp_range_from_bounds(node_start + 1, replacement_end, file);
        if !fidelity.is_exact() {
            return None;
        }
        Some(lsp_range)
    }
}

// completions.go:3709
pub(crate) fn quote_property_name(
    file: P<SourceFile>,
    preferences: &UserPreferences,
    name: &str,
) -> String {
    let r = name.chars().next().unwrap_or('\u{FFFD}');
    if unicode_is_digit(r) {
        return name.to_string();
    }
    quote(file, preferences, name)
}

// Go `unicode.IsDigit` (category Nd). ASCII is exact; outside ASCII `char::is_numeric` (categories Nd, Nl, No)
// stands in, since no Nd table is ported.
fn unicode_is_digit(r: char) -> bool {
    if r.is_ascii() {
        return r.is_ascii_digit();
    }
    r.is_numeric()
}

// Checks whether type is `string & {}`, which is semantically equivalent to string but
// is not reduced by the checker as a special case used for supporting string literal completions
// for string type.
// completions.go:3720
pub(crate) fn is_string_and_empty_anonymous_object_intersection(
    type_checker: &mut Checker,
    t: P<Type>,
) -> bool {
    if !t.is_intersection() {
        return false;
    }

    t.types().len() == 2
        && (are_intersected_types_avoiding_string_reduction(
            type_checker,
            t.types()[0],
            t.types()[1],
        ) || are_intersected_types_avoiding_string_reduction(
            type_checker,
            t.types()[1],
            t.types()[0],
        ))
}

// completions.go:3730
fn are_intersected_types_avoiding_string_reduction(
    type_checker: &mut Checker,
    t1: P<Type>,
    t2: P<Type>,
) -> bool {
    t1.is_string() && type_checker.is_empty_anonymous_object_type(t2)
}

// completions.go:3734
pub(crate) fn escape_snippet_text(text: &str) -> String {
    text.replace('$', "\\$")
}

// completions.go:3738
pub(crate) fn is_named_imports_or_exports(node: P<Node>) -> bool {
    ast::is_named_imports(node) || ast::is_named_exports(node)
}

// completions.go:3742
pub(crate) fn generate_identifier_for_arbitrary_string(text: &str) -> String {
    let mut needs_underscore = false;
    let mut identifier = String::new();

    // Convert "(example, text)" into "_example_text_"
    for (pos, ch) in text.char_indices() {
        let valid_char = if pos == 0 {
            scanner::is_identifier_start(ch as u32)
        } else {
            scanner::is_identifier_part(ch as u32)
        };
        if valid_char {
            if needs_underscore {
                identifier.push('_');
            }
            identifier.push(ch);
            needs_underscore = false;
        } else {
            needs_underscore = true;
        }
    }

    if needs_underscore {
        identifier.push('_');
    }

    // Default to "_" if the provided text was empty
    if identifier.is_empty() {
        return "_".to_string();
    }

    identifier
}

// Copied from vscode TS extension.
// completions.go:3782
fn get_completions_symbol_kind(kind: ScriptElementKind) -> lsproto::CompletionItemKind {
    match kind {
        ScriptElementKind::PrimitiveType | ScriptElementKind::Keyword => {
            lsproto::CompletionItemKind::KEYWORD
        }
        ScriptElementKind::ConstElement
        | ScriptElementKind::LetElement
        | ScriptElementKind::VariableElement
        | ScriptElementKind::LocalVariableElement
        | ScriptElementKind::Alias
        | ScriptElementKind::ParameterElement => lsproto::CompletionItemKind::VARIABLE,

        ScriptElementKind::MemberVariableElement
        | ScriptElementKind::MemberGetAccessorElement
        | ScriptElementKind::MemberSetAccessorElement => lsproto::CompletionItemKind::FIELD,

        ScriptElementKind::FunctionElement | ScriptElementKind::LocalFunctionElement => {
            lsproto::CompletionItemKind::FUNCTION
        }

        ScriptElementKind::MemberFunctionElement
        | ScriptElementKind::ConstructSignatureElement
        | ScriptElementKind::CallSignatureElement
        | ScriptElementKind::IndexSignatureElement => lsproto::CompletionItemKind::METHOD,

        ScriptElementKind::EnumElement => lsproto::CompletionItemKind::ENUM,

        ScriptElementKind::EnumMemberElement => lsproto::CompletionItemKind::ENUM_MEMBER,

        ScriptElementKind::ModuleElement | ScriptElementKind::ExternalModuleName => {
            lsproto::CompletionItemKind::MODULE
        }

        ScriptElementKind::ClassElement | ScriptElementKind::TypeElement => {
            lsproto::CompletionItemKind::CLASS
        }

        ScriptElementKind::InterfaceElement => lsproto::CompletionItemKind::INTERFACE,

        ScriptElementKind::Warning => lsproto::CompletionItemKind::TEXT,

        ScriptElementKind::ScriptElement => lsproto::CompletionItemKind::FILE,

        ScriptElementKind::Directory => lsproto::CompletionItemKind::FOLDER,

        ScriptElementKind::String => lsproto::CompletionItemKind::CONSTANT,

        _ => lsproto::CompletionItemKind::PROPERTY,
    }
}

// Editors will use the `sortText` and then fall back to `name` for sorting, but leave ties in response order.
// So, it's important that we sort those ties in the order we want them displayed if it matters. We don't
// strictly need to sort by name or SortText here since clients are going to do it anyway, but we have to
// do the work of comparing them so we can sort those ties appropriately.
// completions.go:3837
pub fn compare_completion_entries(
    a: &lsproto::CompletionItem,
    b: &lsproto::CompletionItem,
) -> Comparison {
    let compare_strings = stringutil::compare_strings_case_insensitive_then_sensitive;
    let mut result = compare_strings(a.sort_text.as_ref().unwrap(), b.sort_text.as_ref().unwrap());
    if result == stringutil::COMPARISON_EQUAL {
        result = compare_strings(&a.label, &b.label);
    }
    result
}

// completions.go:3846
static KEYWORD_COMPLETIONS_CACHE: OnceLock<Mutex<FxHashMap<u32, Vec<lsproto::CompletionItem>>>> =
    OnceLock::new();

fn keyword_completions_cache() -> &'static Mutex<FxHashMap<u32, Vec<lsproto::CompletionItem>>> {
    KEYWORD_COMPLETIONS_CACHE.get_or_init(Default::default)
}

// completions.go:3848
pub(crate) fn all_keyword_completions() -> &'static [lsproto::CompletionItem] {
    static ALL_KEYWORD_COMPLETIONS: OnceLock<Vec<lsproto::CompletionItem>> = OnceLock::new();
    ALL_KEYWORD_COMPLETIONS.get_or_init(|| {
        let mut result = Vec::with_capacity(
            (Kind::LAST_KEYWORD as i16 - Kind::FIRST_KEYWORD as i16 + 1) as usize,
        );
        result.extend((Kind::FIRST_KEYWORD as i16..=Kind::LAST_KEYWORD as i16).map(|i| {
            lsproto::CompletionItem {
                label: scanner::token_to_string(Kind::from_i16(i)).to_string(),
                kind: Some(lsproto::CompletionItemKind::KEYWORD),
                sort_text: Some(SORT_TEXT_GLOBALS_OR_KEYWORDS.to_string()),
                ..Default::default()
            }
        }));
        result
    })
}

// completions.go:3861
pub(crate) fn clone_items(items: &[lsproto::CompletionItem]) -> Vec<CompletionItem> {
    items.iter().map(|item| CompletionItem::new(item.clone())).collect()
}

// completions.go:3873
pub(crate) fn get_keyword_completions(
    keyword_filter: KeywordCompletionFilters,
    filter_out_ts_only_keywords: bool,
) -> Vec<CompletionItem> {
    if !filter_out_ts_only_keywords {
        return clone_items(&get_typescript_keyword_completions(keyword_filter));
    }

    let index = keyword_filter as u32 + KeywordCompletionFilters::LAST as u32 + 1;
    if let Some(cached) = keyword_completions_cache().lock().unwrap().get(&index) {
        return clone_items(cached);
    }
    let result: Vec<lsproto::CompletionItem> = get_typescript_keyword_completions(keyword_filter)
        .into_iter()
        .filter(|ci| !is_type_script_only_keyword(scanner::string_to_token(&ci.label)))
        .collect();
    let items = clone_items(&result);
    keyword_completions_cache().lock().unwrap().insert(index, result);
    items
}

// completions.go:3892
fn get_typescript_keyword_completions(
    keyword_filter: KeywordCompletionFilters,
) -> Vec<lsproto::CompletionItem> {
    if let Some(cached) = keyword_completions_cache().lock().unwrap().get(&(keyword_filter as u32))
    {
        return cached.clone();
    }
    let result: Vec<lsproto::CompletionItem> = all_keyword_completions()
        .iter()
        .filter(|entry| {
            let kind = scanner::string_to_token(&entry.label);
            match keyword_filter {
                KeywordCompletionFilters::None => false,
                KeywordCompletionFilters::All => {
                    is_function_like_body_keyword(kind)
                        || kind == Kind::DeclareKeyword
                        || kind == Kind::ModuleKeyword
                        || kind == Kind::TypeKeyword
                        || kind == Kind::NamespaceKeyword
                        || kind == Kind::AbstractKeyword
                        || crate::utilities::is_type_keyword(kind) && kind != Kind::UndefinedKeyword
                }
                KeywordCompletionFilters::FunctionLikeBodyKeywords => {
                    is_function_like_body_keyword(kind)
                }
                KeywordCompletionFilters::ClassElementKeywords => {
                    is_class_member_completion_keyword(kind)
                }
                KeywordCompletionFilters::InterfaceElementKeywords => {
                    is_interface_or_type_literal_completion_keyword(kind)
                }
                KeywordCompletionFilters::ConstructorParameterKeywords => {
                    ast::is_parameter_property_modifier(kind)
                }
                KeywordCompletionFilters::TypeAssertionKeywords => {
                    crate::utilities::is_type_keyword(kind) || kind == Kind::ConstKeyword
                }
                KeywordCompletionFilters::TypeKeywords => crate::utilities::is_type_keyword(kind),
                KeywordCompletionFilters::TypeKeyword => kind == Kind::TypeKeyword,
            }
        })
        .cloned()
        .collect();

    keyword_completions_cache().lock().unwrap().insert(keyword_filter as u32, result.clone());
    result
}

// completions.go:3932
fn is_type_script_only_keyword(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::AbstractKeyword
            | Kind::AnyKeyword
            | Kind::BigIntKeyword
            | Kind::BooleanKeyword
            | Kind::DeclareKeyword
            | Kind::EnumKeyword
            | Kind::GlobalKeyword
            | Kind::ImplementsKeyword
            | Kind::InferKeyword
            | Kind::InterfaceKeyword
            | Kind::IsKeyword
            | Kind::KeyOfKeyword
            | Kind::ModuleKeyword
            | Kind::NamespaceKeyword
            | Kind::NeverKeyword
            | Kind::NumberKeyword
            | Kind::ObjectKeyword
            | Kind::OverrideKeyword
            | Kind::PrivateKeyword
            | Kind::ProtectedKeyword
            | Kind::PublicKeyword
            | Kind::ReadonlyKeyword
            | Kind::StringKeyword
            | Kind::SymbolKeyword
            | Kind::TypeKeyword
            | Kind::UniqueKeyword
            | Kind::UnknownKeyword
    )
}

// completions.go:3967
fn is_function_like_body_keyword(kind: Kind) -> bool {
    kind == Kind::AsyncKeyword
        || kind == Kind::AwaitKeyword
        || kind == Kind::UsingKeyword
        || kind == Kind::AsKeyword
        || kind == Kind::SatisfiesKeyword
        || kind == Kind::TypeKeyword
        || !ast::is_contextual_keyword(kind) && !is_class_member_completion_keyword(kind)
}

// completions.go:3977
fn is_class_member_completion_keyword(kind: Kind) -> bool {
    match kind {
        Kind::AbstractKeyword
        | Kind::AccessorKeyword
        | Kind::ConstructorKeyword
        | Kind::GetKeyword
        | Kind::SetKeyword
        | Kind::AsyncKeyword
        | Kind::DeclareKeyword
        | Kind::OverrideKeyword => true,
        _ => ast::is_class_member_modifier(kind),
    }
}

// completions.go:3987
fn is_interface_or_type_literal_completion_keyword(kind: Kind) -> bool {
    kind == Kind::ReadonlyKeyword
}

// completions.go:3991
pub(crate) fn is_contextual_keyword_in_auto_importable_expression_space(keyword: &str) -> bool {
    keyword == "abstract"
        || keyword == "async"
        || keyword == "await"
        || keyword == "declare"
        || keyword == "module"
        || keyword == "namespace"
        || keyword == "type"
        || keyword == "satisfies"
        || keyword == "as"
}

// completions.go:4003
pub(crate) fn get_contextual_keywords(
    file: P<SourceFile>,
    context_token: Option<P<Node>>,
    position: u32,
) -> Vec<lsproto::CompletionItem> {
    let mut entries = Vec::new();
    // An `AssertClause` can come after an import declaration:
    //  import * from "foo" |
    //  import "foo" |
    // or after a re-export declaration that has a module specifier:
    //  export { foo } from "foo" |
    // Source: https://tc39.es/proposal-import-assertions/
    if let Some(context_token) = context_token {
        let parent = context_token.parent().unwrap();
        let token_line = scanner::get_ecma_line_of_position(
            &*file,
            context_token.end().expect_source("parsed tokens have source positions"),
        );
        let current_line = scanner::get_ecma_line_of_position(&*file, position);
        if (ast::is_import_declaration(parent)
            || ast::is_export_declaration(parent) && parent.module_specifier().is_some())
            && Some(context_token) == parent.module_specifier()
            && token_line == current_line
        {
            entries.push(lsproto::CompletionItem {
                label: scanner::token_to_string(Kind::AssertKeyword).to_string(),
                kind: Some(lsproto::CompletionItemKind::KEYWORD),
                sort_text: Some(SORT_TEXT_GLOBALS_OR_KEYWORDS.to_string()),
                ..Default::default()
            });
        }
    }
    entries
}

impl LanguageService {
    // completions.go:4029
    pub(crate) fn get_js_completion_entries(
        _ctx: &Context,
        file: P<SourceFile>,
        position: u32,
        unique_names: &mut FxHashSet<String>,
        mut sorted_entries: Vec<CompletionItem>,
    ) -> Vec<CompletionItem> {
        let name_table = file.get_name_table();
        sorted_entries.extend(name_table.into_iter().filter_map(|(&name, &pos)| {
            // Skip identifiers produced only from the current location
            if matches!(pos, NameTableEntry::Unique(pos) if pos == position ) {
                return None;
            }
            if !unique_names.contains(name)
                && scanner::is_identifier_text(name, LanguageVariant::Standard)
            {
                unique_names.insert(name.to_string());
                Some(CompletionItem::new(lsproto::CompletionItem {
                    label: name.to_string(),
                    kind: Some(lsproto::CompletionItemKind::TEXT),
                    sort_text: Some(SORT_TEXT_JAVASCRIPT_IDENTIFIERS.to_string()),
                    commit_characters: Some(Vec::new()),
                    ..Default::default()
                }))
            } else {
                None
            }
        }));
        sorted_entries
    }

    // completions.go:4057
    pub(crate) fn get_optional_replacement_span(
        &self,
        location: Option<P<Node>>,
        file: P<SourceFile>,
    ) -> Option<lsproto::Range> {
        // StringLiteralLike locations are handled separately in stringCompletions.ts
        if let Some(location) = location
            && (location.kind() == Kind::Identifier || location.kind() == Kind::PrivateIdentifier)
        {
            let start = astnav::get_start_of_node(location, file, false /*includeJSDoc*/);
            let (lsp_range, fidelity) = self.create_lsp_range_from_bounds(
                start,
                location.end().expect_source("parsed names have source positions"),
                file,
            );
            if fidelity.is_exact() {
                return Some(lsp_range);
            }
        }
        None
    }
}

// completions.go:4069
pub(crate) fn is_member_completion_kind(kind: CompletionKind) -> bool {
    kind == CompletionKind::ObjectPropertyDeclaration
        || kind == CompletionKind::MemberLike
        || kind == CompletionKind::PropertyAccess
}

// completions.go:4075
pub(crate) fn try_get_function_like_body_completion_container(
    context_token: Option<P<Node>>,
) -> Option<P<Node>> {
    let context_token = context_token?;

    let mut prev: Option<P<Node>> = None;
    ast::find_ancestor_or_quit(context_token, |node| {
        if ast::is_class_like(node) {
            return FindAncestorResult::Quit;
        }
        if ast::is_function_like_declaration(node) && prev == node.body() {
            return FindAncestorResult::True;
        }
        prev = Some(node);
        FindAncestorResult::False
    })
}

// completions.go:4094
pub(crate) fn compute_commit_characters_and_is_new_identifier(
    context_token: Option<P<Node>>,
    file: P<SourceFile>,
    position: u32,
) -> (bool, Vec<String>) {
    let Some(context_token) = context_token else {
        return (false, strings(&ALL_COMMIT_CHARACTERS));
    };
    let containing_node_kind = context_token.parent().unwrap().kind();
    let token_kind = keyword_for_node(context_token);
    // Previous token may have been a keyword that was converted to an identifier.
    match token_kind {
        Kind::CommaToken => {
            return match containing_node_kind {
                // func( a, |
                // new C(a, |
                Kind::CallExpression | Kind::NewExpression => {
                    let expression = context_token.parent().unwrap().expression().unwrap();
                    // func\n(a, |
                    if get_line_of_position(
                        file,
                        expression.end().expect_source("parsed expressions have source positions"),
                    ) != get_line_of_position(file, position)
                    {
                        return (true, strings(&NO_COMMA_COMMIT_CHARACTERS));
                    }
                    (true, strings(&ALL_COMMIT_CHARACTERS))
                }
                // const x = (a, |
                Kind::BinaryExpression => (true, strings(&NO_COMMA_COMMIT_CHARACTERS)),
                // constructor( a, | /* public, protected, private keywords are allowed here, so show completion */
                // var x: (s: string, list|
                // const obj = { x, |
                Kind::Constructor | Kind::FunctionType | Kind::ObjectLiteralExpression => {
                    (true, strings(&EMPTY_COMMIT_CHARACTERS))
                }
                // [a, |
                Kind::ArrayLiteralExpression => (true, strings(&ALL_COMMIT_CHARACTERS)),
                _ => (false, strings(&ALL_COMMIT_CHARACTERS)),
            };
        }
        Kind::OpenParenToken => {
            return match containing_node_kind {
                // func( |
                // new C(a|
                Kind::CallExpression | Kind::NewExpression => {
                    let expression = context_token.parent().unwrap().expression().unwrap();
                    // func\n( |
                    if get_line_of_position(
                        file,
                        expression.end().expect_source("parsed expressions have source positions"),
                    ) != get_line_of_position(file, position)
                    {
                        return (true, strings(&NO_COMMA_COMMIT_CHARACTERS));
                    }
                    (true, strings(&ALL_COMMIT_CHARACTERS))
                }
                // const x = (a|
                Kind::ParenthesizedExpression => (true, strings(&NO_COMMA_COMMIT_CHARACTERS)),
                // constructor( |
                // function F(pred: (a| /* this can become an arrow function, where 'a' is the argument */
                Kind::Constructor | Kind::ParenthesizedType => {
                    (true, strings(&EMPTY_COMMIT_CHARACTERS))
                }
                _ => (false, strings(&ALL_COMMIT_CHARACTERS)),
            };
        }
        Kind::OpenBracketToken => {
            return match containing_node_kind {
                // [ |
                // [ | : string ]
                // [ | : string ]
                // [ |    /* this can become an index signature */
                Kind::ArrayLiteralExpression
                | Kind::IndexSignature
                | Kind::TupleType
                | Kind::ComputedPropertyName => (true, strings(&ALL_COMMIT_CHARACTERS)),
                _ => (false, strings(&ALL_COMMIT_CHARACTERS)),
            };
        }
        // module |
        // namespace |
        // import |
        Kind::ModuleKeyword | Kind::NamespaceKeyword | Kind::ImportKeyword => {
            return (true, strings(&EMPTY_COMMIT_CHARACTERS));
        }
        Kind::DotToken => {
            return match containing_node_kind {
                // module A.|
                Kind::ModuleDeclaration => (true, strings(&EMPTY_COMMIT_CHARACTERS)),
                _ => (false, strings(&ALL_COMMIT_CHARACTERS)),
            };
        }
        Kind::OpenBraceToken => {
            return match containing_node_kind {
                // class A { |
                // const obj = { |
                Kind::ClassDeclaration | Kind::ObjectLiteralExpression => {
                    (true, strings(&EMPTY_COMMIT_CHARACTERS))
                }
                _ => (false, strings(&ALL_COMMIT_CHARACTERS)),
            };
        }
        Kind::EqualsToken => {
            return match containing_node_kind {
                // const x = a|
                // x = a|
                Kind::VariableDeclaration | Kind::BinaryExpression => {
                    (true, strings(&ALL_COMMIT_CHARACTERS))
                }
                _ => (false, strings(&ALL_COMMIT_CHARACTERS)),
            };
        }
        Kind::TemplateHead => {
            // `aa ${|
            return (
                containing_node_kind == Kind::TemplateExpression,
                strings(&ALL_COMMIT_CHARACTERS),
            );
        }
        Kind::TemplateMiddle => {
            // `aa ${10} dd ${|
            return (containing_node_kind == Kind::TemplateSpan, strings(&ALL_COMMIT_CHARACTERS));
        }
        Kind::AsyncKeyword => {
            // const obj = { async c|()
            // const obj = { async c|
            if containing_node_kind == Kind::MethodDeclaration
                || containing_node_kind == Kind::ShorthandPropertyAssignment
            {
                return (true, strings(&EMPTY_COMMIT_CHARACTERS));
            }
            return (false, strings(&ALL_COMMIT_CHARACTERS));
        }
        Kind::AsteriskToken => {
            // const obj = { * c|
            if containing_node_kind == Kind::MethodDeclaration {
                return (true, strings(&EMPTY_COMMIT_CHARACTERS));
            }
            return (false, strings(&ALL_COMMIT_CHARACTERS));
        }
        _ => {}
    }

    if is_class_member_completion_keyword(token_kind) {
        return (true, strings(&EMPTY_COMMIT_CHARACTERS));
    }

    (false, strings(&ALL_COMMIT_CHARACTERS))
}

// completions.go:4222
fn keyword_for_node(node: P<Node>) -> Kind {
    if ast::is_identifier(node) {
        return scanner::identifier_to_keyword_kind(node);
    }
    node.kind()
}

// Finds the first node that "embraces" the position, so that one may
// accurately aggregate locals from the closest containing scope.
// completions.go:4231
pub(crate) fn get_scope_node(
    initial_token: Option<P<Node>>,
    position: u32,
    file: P<SourceFile>,
) -> Option<P<Node>> {
    let mut scope = initial_token;
    while let Some(s) = scope {
        if position_belongs_to_node(s, position, file) {
            break;
        }
        scope = s.parent();
    }
    scope
}

// Determines if a type is exactly the same type resolved by the global 'self', 'global', or 'globalThis'.
// completions.go:4252
pub(crate) fn is_probably_global_type(
    t: P<Type>,
    file: P<SourceFile>,
    type_checker: &mut Checker,
) -> bool {
    // The type of `self` and `window` is the same in lib.dom.d.ts, but `window` does not exist in
    // lib.webworker.d.ts, so checking against `self` is also a check against `window` when it exists.
    let self_symbol = type_checker.get_global_symbol_exported(
        "self",
        SymbolFlags::Value,
        None, /*diagnostic*/
    );
    if let Some(self_symbol) = self_symbol
        && type_checker.get_type_of_symbol_at_location(self_symbol, Some(file.as_node())) == Some(t)
    {
        return true;
    }
    let global_symbol = type_checker.get_global_symbol_exported(
        "global",
        SymbolFlags::Value,
        None, /*diagnostic*/
    );
    if let Some(global_symbol) = global_symbol
        && type_checker.get_type_of_symbol_at_location(global_symbol, Some(file.as_node()))
            == Some(t)
    {
        return true;
    }
    let global_this_symbol = type_checker.get_global_symbol_exported(
        "globalThis",
        SymbolFlags::Value,
        None, /*diagnostic*/
    );
    if let Some(global_this_symbol) = global_this_symbol
        && type_checker.get_type_of_symbol_at_location(global_this_symbol, Some(file.as_node()))
            == Some(t)
    {
        return true;
    }
    false
}

// completions.go:4270
pub(crate) fn try_get_type_literal_node(node: Option<P<Node>>) -> Option<P<Node>> {
    let node = node?;

    let parent = node.parent().unwrap();
    match node.kind() {
        Kind::OpenBraceToken => {
            if ast::is_type_literal_node(parent) {
                return Some(parent);
            }
        }
        Kind::SemicolonToken | Kind::CommaToken | Kind::Identifier
            if parent.kind() == Kind::PropertySignature
                && ast::is_type_literal_node(parent.parent().unwrap()) =>
        {
            return parent.parent();
        }
        _ => {}
    }

    None
}

// completions.go:4290
pub(crate) fn get_constraint_of_type_argument_property(
    node: Option<P<Node>>,
    type_checker: &mut Checker,
) -> Option<P<Type>> {
    let node = node?;

    if ast::is_type_node(node) {
        let constraint = type_checker.get_type_argument_constraint_exported(node);
        if constraint.is_some() {
            return constraint;
        }
    }

    let t = get_constraint_of_type_argument_property(node.parent(), type_checker)?;

    match node.kind() {
        Kind::PropertySignature => {
            // Try to get the reparsed node first - we may be in JSDoc.
            let reparsed = ast::get_reparsed_node_for_node(node).unwrap();
            if let Some(symbol) = reparsed.symbol() {
                return type_checker
                    .get_type_of_property_of_contextual_type_exported(t, symbol.name());
            }

            // In some cases, we won't have a corresponding symbol
            // (e.g. JSDoc types that never get re-attached) so we'll use
            // the name as declared by the property as a best-effort.
            if let Some(name) = ast::try_get_text_of_property_name(reparsed.name().unwrap()) {
                return type_checker.get_type_of_property_of_contextual_type_exported(t, &name);
            }

            None
        }
        Kind::ColonToken => {
            if node.parent().unwrap().kind() == Kind::PropertySignature {
                // The cursor is at a property value location like `Foo<{ x: | }`.
                // `t` already refers to the appropriate property type.
                return Some(t);
            }
            None
        }
        Kind::IntersectionType | Kind::TypeLiteral | Kind::UnionType => Some(t),
        Kind::OpenBracketToken => type_checker.get_element_type_of_array_type_exported(t),
        _ => None,
    }
}

// completions.go:4338
pub(crate) fn try_get_object_like_completion_container(
    context_token: Option<P<Node>>,
    position: u32,
    file: P<SourceFile>,
) -> Option<P<Node>> {
    let context_token = context_token?;

    let parent = context_token.parent().unwrap();
    match context_token.kind() {
        // const x = { |
        // const x = { a: 0, |
        Kind::OpenBraceToken | Kind::CommaToken => {
            if ast::is_object_literal_expression(parent) || ast::is_object_binding_pattern(parent) {
                return Some(parent);
            }
        }
        Kind::AsteriskToken => {
            if ast::is_method_declaration(parent)
                && ast::is_object_literal_expression(parent.parent().unwrap())
            {
                return parent.parent();
            }
        }
        Kind::AsyncKeyword => {
            if ast::is_object_literal_expression(parent.parent().unwrap()) {
                return parent.parent();
            }
        }
        Kind::Identifier => {
            if context_token.text() == "async" && ast::is_shorthand_property_assignment(parent) {
                return parent.parent();
            }
            if ast::is_object_literal_expression(parent.parent().unwrap())
                && (ast::is_spread_assignment(parent)
                    || ast::is_shorthand_property_assignment(parent)
                        && get_line_of_position(
                            file,
                            context_token
                                .end()
                                .expect_source("parsed tokens have source positions"),
                        ) != get_line_of_position(file, position))
            {
                return parent.parent();
            }
            let ancestor_node = ast::find_ancestor(parent, ast::is_property_assignment);
            if let Some(ancestor_node) = ancestor_node
                && lsutil::get_last_token(Some(ancestor_node), file) == Some(context_token)
                && ast::is_object_literal_expression(ancestor_node.parent().unwrap())
            {
                return ancestor_node.parent();
            }
        }
        _ => {
            if parent.parent().is_some_and(|pp| pp.parent().is_some())
                && (ast::is_method_declaration(parent.parent().unwrap())
                    || ast::is_get_accessor_declaration(parent.parent().unwrap())
                    || ast::is_set_accessor_declaration(parent.parent().unwrap()))
                && ast::is_object_literal_expression(parent.parent().unwrap().parent().unwrap())
            {
                return parent.parent().unwrap().parent();
            }
            if ast::is_spread_assignment(parent)
                && ast::is_object_literal_expression(parent.parent().unwrap())
            {
                return parent.parent();
            }
            let ancestor_node = ast::find_ancestor(parent, ast::is_property_assignment);
            if context_token.kind() != Kind::ColonToken
                && let Some(ancestor_node) = ancestor_node
                && lsutil::get_last_token(Some(ancestor_node), file) == Some(context_token)
                && ast::is_object_literal_expression(ancestor_node.parent().unwrap())
            {
                return ancestor_node.parent();
            }
        }
    }

    None
}

// completions.go:4396
pub(crate) fn try_get_object_literal_contextual_type(
    node: P<Node>,
    type_checker: &mut Checker,
) -> Option<P<Type>> {
    let t = type_checker.get_contextual_type_exported(node, ContextFlags::None);
    if t.is_some() {
        return t;
    }

    let parent = ast::walk_up_parenthesized_expressions(node.parent()).unwrap();
    if ast::is_binary_expression(parent)
        && parent.as_binary_expression().operator_token.kind() == Kind::EqualsToken
        && node == parent.as_binary_expression().left
    {
        // Object literal is assignment pattern: ({ | } = x)
        return Some(type_checker.get_type_at_location(parent));
    }
    if ast::is_expression(parent) {
        // f(() => (({ | })));
        return type_checker.get_contextual_type_exported(parent, ContextFlags::None);
    }

    None
}

// completions.go:4417
pub(crate) fn get_properties_for_object_expression(
    contextual_type: P<Type>,
    completions_type: Option<P<Type>>,
    obj: P<Node>,
    type_checker: &mut Checker,
) -> Vec<P<Symbol>> {
    let has_completions_type =
        completions_type.is_some() && completions_type != Some(contextual_type);
    let types: Vec<P<Type>> = if contextual_type.is_union() {
        contextual_type.types().to_vec()
    } else {
        vec![contextual_type]
    };
    let filtered: Vec<P<Type>> = types
        .into_iter()
        .filter(|&t| type_checker.get_promised_type_of_promise(t).is_none())
        .collect();
    let promise_filtered_contextual_type = type_checker.get_union_type_exported(&filtered);

    let t = if has_completions_type
        && !completions_type.unwrap().flags().intersects(TypeFlags::AnyOrUnknown)
    {
        type_checker
            .get_union_type_exported(&[promise_filtered_contextual_type, completions_type.unwrap()])
    } else {
        promise_filtered_contextual_type
    };

    // Filter out members whose only declaration is the object literal itself to avoid
    // self-fulfilling completions like:
    //
    // function f<T>(x: T) {}
    // f({ abc/**/: "" }) // `abc` is a member of `T` but only because it declares itself
    let has_declaration_other_than_self = |member: &P<Symbol>| -> bool {
        if member.declarations().is_empty() {
            return true;
        }
        member.declarations().iter().any(|decl| decl.parent() != Some(obj))
    };

    let properties = get_apparent_properties(t, obj, type_checker);
    if t.is_class() && contains_non_public_properties(&properties) {
        Vec::new()
    } else if has_completions_type {
        properties.into_iter().filter(has_declaration_other_than_self).collect()
    } else {
        properties
    }
}

// completions.go:4463
fn get_apparent_properties(
    t: P<Type>,
    node: P<Node>,
    type_checker: &mut Checker,
) -> Vec<P<Symbol>> {
    if !t.is_union() {
        return type_checker.get_apparent_properties(t);
    }
    let mut filtered: Vec<P<Type>> = Vec::new();
    filtered.extend(t.types().iter().filter_map(|&member_type| {
        let excluded = member_type.flags().intersects(TypeFlags::Primitive)
            || type_checker.is_array_like_type_exported(member_type)
            || type_checker.is_type_invalid_due_to_union_discriminant(member_type, node)
            || type_checker.type_has_call_or_construct_signatures_exported(member_type)
            || member_type.is_class() && {
                let apparent = type_checker.get_apparent_properties(member_type);
                contains_non_public_properties(&apparent)
            };
        if excluded { None } else { Some(member_type) }
    }));
    type_checker.get_all_possible_properties_of_types(&filtered)
}

// completions.go:4476
fn contains_non_public_properties(props: &[P<Symbol>]) -> bool {
    props.iter().any(|&p| {
        checker::get_declaration_modifier_flags_from_symbol_exported(p)
            .intersects(ModifierFlags::NonPublicAccessibilityModifier)
    })
}

// Filters out members that are already declared in the object literal or binding pattern.
// Also computes the set of existing members declared by spread assignment.
// completions.go:4484
pub(crate) fn filter_object_members_list(
    contextual_member_symbols: &[P<Symbol>],
    existing_members: &[P<Node>],
    file: P<SourceFile>,
    position: u32,
    type_checker: &mut Checker,
) -> (Vec<P<Symbol>>, FxHashSet<String>) {
    if existing_members.is_empty() {
        return (contextual_member_symbols.to_vec(), FxHashSet::default());
    }

    let mut members_declared_by_spread_assignment: FxHashSet<String> = FxHashSet::default();
    let mut existing_member_names: FxHashSet<String> = FxHashSet::default();
    for &member in existing_members {
        // Ignore omitted expressions for missing members.
        if member.kind() != Kind::PropertyAssignment
            && member.kind() != Kind::ShorthandPropertyAssignment
            && member.kind() != Kind::BindingElement
            && member.kind() != Kind::MethodDeclaration
            && member.kind() != Kind::GetAccessor
            && member.kind() != Kind::SetAccessor
            && member.kind() != Kind::SpreadAssignment
        {
            continue;
        }

        // If this is the current item we are editing right now, do not filter it out.
        if is_currently_editing_node(member, file, position) {
            continue;
        }

        let mut existing_name = String::new();

        if ast::is_spread_assignment(member) {
            set_member_declared_by_spread_assignment(
                member,
                &mut members_declared_by_spread_assignment,
                type_checker,
            );
        } else if ast::is_binding_element(member) && member.property_name().is_some() {
            // include only identifiers in completion list
            if member.property_name().unwrap().kind() == Kind::Identifier {
                existing_name = member.property_name().unwrap().text().to_string();
            }
        } else {
            // TODO: Account for computed property name
            // NOTE: if one only performs this step when m.name is an identifier,
            // things like '__proto__' are not filtered out.
            let name = ast::get_name_of_declaration(member);
            if let Some(name) = name
                && ast::is_property_name_literal(name)
            {
                existing_name = name.text().to_string();
            }
        }

        if !existing_name.is_empty() {
            existing_member_names.insert(existing_name);
        }
    }

    let filtered_symbols = contextual_member_symbols
        .iter()
        .copied()
        .filter(|m| !existing_member_names.contains(m.name()))
        .collect();

    (filtered_symbols, members_declared_by_spread_assignment)
}

// completions.go:4545
pub(crate) fn is_currently_editing_node(node: P<Node>, file: P<SourceFile>, position: u32) -> bool {
    let start = astnav::get_start_of_node(node, file, false /*includeJSDoc*/);
    start <= position && position <= node.end().expect_source("parsed nodes have source positions")
}

// completions.go:4550
fn set_member_declared_by_spread_assignment(
    declaration: P<Node>,
    members: &mut FxHashSet<String>,
    type_checker: &mut Checker,
) {
    let expression = declaration.expression().unwrap();
    let symbol = type_checker.get_symbol_at_location_exported(expression);
    let mut t: Option<P<Type>> = None;
    if let Some(symbol) = symbol {
        t = type_checker.get_type_of_symbol_at_location(symbol, Some(expression));
    }
    let mut properties: &[P<Symbol>] = &[];
    if let Some(t) = t
        && t.flags().intersects(TypeFlags::StructuredType)
    {
        properties = t.as_structured_type().properties();
    }
    members.extend(properties.iter().map(|property| property.name().to_string()));
}

// Returns the immediate owning class declaration of a context token,
// on the condition that one exists and that the context implies completion should be given.
// completions.go:4568
pub(crate) fn try_get_constructor_like_completion_container(
    context_token: Option<P<Node>>,
) -> Option<P<Node>> {
    let context_token = context_token?;

    let parent = context_token.parent().unwrap();
    match context_token.kind() {
        Kind::OpenParenToken | Kind::CommaToken => {
            if ast::is_constructor_declaration(parent) {
                return Some(parent);
            }
            None
        }
        _ => {
            if is_constructor_parameter_completion(context_token) {
                return parent.parent();
            }
            None
        }
    }
}

// completions.go:4588
fn is_constructor_parameter_completion(node: P<Node>) -> bool {
    node.parent().is_some_and(|parent| {
        ast::is_parameter_declaration(parent)
            && ast::is_constructor_declaration(parent.parent().unwrap())
            && (ast::is_parameter_property_modifier(node.kind()) || ast::is_declaration_name(node))
    })
}

// Returns the immediate owning class declaration of a context token,
// on the condition that one exists and that the context implies completion should be given.
// completions.go:4595
pub(crate) fn try_get_object_type_declaration_completion_container(
    file: P<SourceFile>,
    context_token: Option<P<Node>>,
    location: P<Node>,
    position: u32,
) -> Option<P<Node>> {
    // class c { method() { } | method2() { } }
    match location.kind() {
        Kind::SyntaxList => {
            if ast::is_object_type_declaration(location.parent().unwrap()) {
                return location.parent();
            }
            return None;
        }
        Kind::EndOfFile => {
            let stmt_list = location.parent().unwrap().statement_list();
            if let Some(stmt_list) = stmt_list {
                let nodes = stmt_list.nodes();
                if !nodes.is_empty() && ast::is_object_type_declaration(nodes[nodes.len() - 1]) {
                    let cls = nodes[nodes.len() - 1];
                    if astnav::find_child_of_kind(cls, Kind::CloseBraceToken, file).is_none() {
                        return Some(cls);
                    }
                }
            }
        }
        Kind::PrivateIdentifier => {
            if ast::is_property_declaration(location.parent().unwrap()) {
                return ast::find_ancestor(location, ast::is_class_like);
            }
        }
        Kind::Identifier => {
            let original_keyword_kind = scanner::identifier_to_keyword_kind(location);
            if original_keyword_kind != Kind::Unknown {
                return None;
            }
            // class c { public prop = c| }
            if ast::is_property_declaration(location.parent().unwrap())
                && location.parent().unwrap().initializer() == Some(location)
            {
                return None;
            }
            // class c extends React.Component { a: () => 1\n compon| }
            if is_from_object_type_declaration(location) {
                return ast::find_ancestor(location, ast::is_object_type_declaration);
            }
        }
        _ => {}
    }

    let context_token = context_token?;

    // class C { blah; constructor/**/ }
    // or
    // class C { blah \n constructor/**/ }
    if location.kind() == Kind::ConstructorKeyword
        || (ast::is_identifier(context_token)
            && ast::is_property_declaration(context_token.parent().unwrap())
            && ast::is_class_like(location))
    {
        return ast::find_ancestor(context_token, ast::is_class_like);
    }

    match context_token.kind() {
        // class c { public prop = | /* global completions */ }
        Kind::EqualsToken => None,
        // class c {getValue(): number; | }
        // class c { method() { } | }
        Kind::SemicolonToken | Kind::CloseBraceToken => {
            // class c { method() { } b| }
            if is_from_object_type_declaration(location)
                && location.parent().unwrap().name() == Some(location)
            {
                return location.parent().unwrap().parent();
            }
            if ast::is_object_type_declaration(location) {
                return Some(location);
            }
            None
        }
        // class c { |
        // class c {getValue(): number, | }
        Kind::OpenBraceToken | Kind::CommaToken => {
            if ast::is_object_type_declaration(context_token.parent().unwrap()) {
                return context_token.parent();
            }
            None
        }
        _ => {
            if ast::is_object_type_declaration(location) {
                // class C extends React.Component { a: () => 1\n| }
                // class C { prop = ""\n | }
                if get_line_of_position(
                    file,
                    context_token.end().expect_source("parsed tokens have source positions"),
                ) != get_line_of_position(file, position)
                {
                    return Some(location);
                }
                let is_valid_keyword: fn(Kind) -> bool =
                    if ast::is_class_like(context_token.parent().unwrap().parent().unwrap()) {
                        is_class_member_completion_keyword
                    } else {
                        is_interface_or_type_literal_completion_keyword
                    };

                if is_valid_keyword(context_token.kind())
                    || context_token.kind() == Kind::AsteriskToken
                    || ast::is_identifier(context_token)
                        && is_valid_keyword(scanner::identifier_to_keyword_kind(context_token))
                {
                    return context_token.parent().unwrap().parent();
                }
            }

            None
        }
    }
}

// completions.go:4692
fn is_from_object_type_declaration(node: P<Node>) -> bool {
    node.parent().is_some_and(|parent| {
        ast::is_class_or_type_element(parent)
            && ast::is_object_type_declaration(parent.parent().unwrap())
    })
}

// Filters out completion suggestions for class elements.
// completions.go:4697
pub(crate) fn filter_class_members_list(
    base_symbols: &[P<Symbol>],
    existing_members: &[P<Node>],
    class_element_modifier_flags: ModifierFlags,
    file: P<SourceFile>,
    position: u32,
) -> Vec<P<Symbol>> {
    let mut existing_member_names: FxHashSet<String> = FxHashSet::default();
    existing_member_names.extend(existing_members.iter().filter_map(|&member| {
        // Ignore omitted expressions for missing members.
        if member.kind() != Kind::PropertyDeclaration
            && member.kind() != Kind::MethodDeclaration
            && member.kind() != Kind::GetAccessor
            && member.kind() != Kind::SetAccessor
        {
            return None;
        }

        // If this is the current item we are editing right now, do not filter it out
        if is_currently_editing_node(member, file, position) {
            return None;
        }

        // Don't filter member even if the name matches if it is declared private in the list.
        if member.modifier_flags().intersects(ModifierFlags::Private) {
            return None;
        }

        // Do not filter it out if the static presence doesn't match.
        if ast::is_static(member) != class_element_modifier_flags.intersects(ModifierFlags::Static)
        {
            return None;
        }

        let existing_name = ast::get_property_name_for_property_name_node(member.name().unwrap());
        if existing_name.is_empty() { None } else { Some(existing_name.into_owned()) }
    }));

    base_symbols
        .iter()
        .copied()
        .filter(|&property_symbol| {
            !existing_member_names.contains(ast::symbol_name(property_symbol))
                && !property_symbol.declarations().is_empty()
                && !checker::get_declaration_modifier_flags_from_symbol_exported(property_symbol)
                    .intersects(ModifierFlags::Private)
                && property_symbol.value_declaration().is_none_or(|declaration| {
                    !ast::is_private_identifier_class_element_declaration(declaration)
                })
        })
        .collect()
}

// completions.go:4743
pub(crate) fn try_get_containing_jsx_element(
    context_token: Option<P<Node>>,
    file: P<SourceFile>,
) -> Option<P<Node>> {
    let context_token = context_token?;

    let parent = context_token.parent();
    match context_token.kind() {
        Kind::GreaterThanToken
        | Kind::LessThanSlashToken
        | Kind::SlashToken
        | Kind::Identifier
        | Kind::PropertyAccessExpression
        | Kind::JsxNamespacedName
        | Kind::JsxAttributes
        | Kind::JsxAttribute
        | Kind::JsxSpreadAttribute => {
            if let Some(parent) = parent {
                if parent.kind() == Kind::JsxSelfClosingElement
                    || parent.kind() == Kind::JsxOpeningElement
                {
                    if context_token.kind() == Kind::GreaterThanToken {
                        let preceding_token = astnav::find_preceding_token(
                            file,
                            context_token
                                .pos()
                                .expect_source("parsed tokens have source positions"),
                        );
                        if parent.type_arguments().is_empty()
                            || preceding_token.is_some_and(|t| t.kind() == Kind::SlashToken)
                        {
                            return None;
                        }
                    }
                    return Some(parent);
                } else if ast::is_jsx_namespaced_name(parent)
                    && parent.parent().is_some_and(|pp| {
                        pp.kind() == Kind::JsxSelfClosingElement
                            || pp.kind() == Kind::JsxOpeningElement
                    })
                {
                    return parent.parent();
                } else if parent.kind() == Kind::JsxAttribute {
                    // Currently we parse JsxOpeningLikeElement as:
                    //      JsxOpeningLikeElement
                    //          attributes: JsxAttributes
                    //             properties: NodeArray<JsxAttributeLike>
                    return parent.parent().unwrap().parent();
                }
            }
        }
        // The context token is the closing } or " of an attribute, which means
        // its parent is a JsxExpression, whose parent is a JsxAttribute,
        // whose parent is a JsxOpeningLikeElement
        Kind::StringLiteral => {
            if let Some(parent) = parent
                && (parent.kind() == Kind::JsxAttribute
                    || parent.kind() == Kind::JsxSpreadAttribute)
            {
                // Currently we parse JsxOpeningLikeElement as:
                //      JsxOpeningLikeElement
                //          attributes: JsxAttributes
                //             properties: NodeArray<JsxAttributeLike>
                return parent.parent().unwrap().parent();
            }
        }
        Kind::CloseBraceToken => {
            if let Some(parent) = parent {
                if parent.kind() == Kind::JsxExpression
                    && parent.parent().is_some_and(|pp| pp.kind() == Kind::JsxAttribute)
                {
                    // Currently we parse JsxOpeningLikeElement as:
                    //      JsxOpeningLikeElement
                    //          attributes: JsxAttributes
                    //             properties: NodeArray<JsxAttributeLike>
                    //                  each JsxAttribute can have initializer as JsxExpression
                    return parent.parent().unwrap().parent().unwrap().parent();
                }
                if parent.kind() == Kind::JsxSpreadAttribute {
                    // Currently we parse JsxOpeningLikeElement as:
                    //      JsxOpeningLikeElement
                    //          attributes: JsxAttributes
                    //             properties: NodeArray<JsxAttributeLike>
                    return parent.parent().unwrap().parent();
                }
            }
        }
        _ => {}
    }

    None
}

// Filters out completion suggestions from 'symbols' according to existing JSX attributes.
// @returns Symbols to be suggested in a JSX element, barring those whose attributes
// do not occur at the current position and have not otherwise been typed.
// completions.go:4807
pub(crate) fn filter_jsx_attributes(
    symbols: &[P<Symbol>],
    attributes: &[P<Node>],
    file: P<SourceFile>,
    position: u32,
    type_checker: &mut Checker,
) -> (Vec<P<Symbol>>, FxHashSet<String>) {
    let mut existing_names: FxHashSet<&'static str> = FxHashSet::default();
    let mut members_declared_by_spread_assignment: FxHashSet<String> = FxHashSet::default();
    for &attr in attributes {
        // If this is the item we are editing right now, do not filter it out.
        if is_currently_editing_node(attr, file, position) {
            continue;
        }

        if attr.kind() == Kind::JsxAttribute {
            existing_names.insert(attr.name().unwrap().text());
        } else if ast::is_jsx_spread_attribute(attr) {
            set_member_declared_by_spread_assignment(
                attr,
                &mut members_declared_by_spread_assignment,
                type_checker,
            );
        }
    }

    (
        symbols.iter().copied().filter(|a| !existing_names.contains(a.name())).collect(),
        members_declared_by_spread_assignment,
    )
}

// completions.go:4833
pub(crate) fn is_type_keyword_token_or_identifier(node: P<Node>) -> bool {
    ast::is_type_keyword_token(node)
        || ast::is_identifier(node)
            && scanner::identifier_to_keyword_kind(node) == Kind::TypeKeyword
}

impl LanguageService {
    // Returns the item defaults for completion items, if that capability is supported.
    // Otherwise, if some item default is not supported by client, sets that property on each item.
    // completions.go:4840
    pub(crate) fn set_item_defaults(
        &self,
        ctx: &Context,
        position: u32,
        file: P<SourceFile>,
        items: &mut [CompletionItem],
        default_commit_characters: Option<&[String]>,
        optional_replacement_span: Option<lsproto::Range>,
    ) -> Option<lsproto::CompletionItemDefaults> {
        let mut item_defaults: Option<lsproto::CompletionItemDefaults> = None;
        if let Some(default_commit_characters) = default_commit_characters {
            let supports_item_commit_characters = client_supports_item_commit_characters(ctx);
            if client_supports_default_commit_characters(ctx) && supports_item_commit_characters {
                item_defaults = Some(lsproto::CompletionItemDefaults {
                    commit_characters: Some(default_commit_characters.to_vec()),
                    ..Default::default()
                });
            } else if supports_item_commit_characters {
                for item in items.iter_mut() {
                    if item.commit_characters.is_none() {
                        item.commit_characters = Some(default_commit_characters.to_vec());
                    }
                }
            }
        }
        if let Some(optional_replacement_span) = optional_replacement_span {
            // Ported from vscode ts extension.
            let (end, fidelity) = self.create_lsp_position(position, file);
            if !fidelity.is_exact() {
                return item_defaults;
            }
            let insert_range = lsproto::Range { start: optional_replacement_span.start, end };
            if client_supports_default_edit_range(ctx) {
                let defaults = item_defaults.get_or_insert_with(Default::default);
                defaults.edit_range = Some(lsproto::RangeOrEditRangeWithInsertReplace {
                    edit_range_with_insert_replace: Some(lsproto::EditRangeWithInsertReplace {
                        insert: insert_range,
                        replace: optional_replacement_span,
                    }),
                    ..Default::default()
                });
                for item in items.iter_mut() {
                    // If `editRange` is set, `insertText` is ignored by the client, so we need to
                    // provide `textEdit` instead.
                    if item.insert_text.is_some() && item.text_edit.is_none() {
                        item.text_edit = Some(lsproto::TextEditOrInsertReplaceEdit {
                            insert_replace_edit: Some(lsproto::InsertReplaceEdit {
                                new_text: item.insert_text.clone().unwrap(),
                                insert: insert_range,
                                replace: optional_replacement_span,
                            }),
                            ..Default::default()
                        });
                        item.insert_text = None;
                    }
                }
            } else if client_supports_item_insert_replace(ctx) {
                for item in items.iter_mut() {
                    if item.text_edit.is_none() {
                        let new_text =
                            item.insert_text.clone().unwrap_or_else(|| item.label.clone());
                        item.text_edit = Some(lsproto::TextEditOrInsertReplaceEdit {
                            insert_replace_edit: Some(lsproto::InsertReplaceEdit {
                                new_text,
                                insert: insert_range,
                                replace: optional_replacement_span,
                            }),
                            ..Default::default()
                        });
                    }
                }
            }
        }

        item_defaults
    }

    // completions.go:4913
    pub(crate) fn specific_keyword_completion_info(
        &self,
        ctx: &Context,
        position: u32,
        file: P<SourceFile>,
        mut items: Vec<CompletionItem>,
        is_new_identifier_location: bool,
        optional_replacement_span: Option<lsproto::Range>,
    ) -> CompletionList {
        let default_commit_characters = get_default_commit_characters(is_new_identifier_location);
        let item_defaults = self.set_item_defaults(
            ctx,
            position,
            file,
            &mut items,
            Some(&default_commit_characters),
            optional_replacement_span,
        );
        CompletionList { is_incomplete: false, item_defaults, items, ..Default::default() }
    }

    // completions.go:4937
    pub(crate) fn get_jsx_closing_tag_completion(
        &self,
        ctx: &Context,
        location: P<Node>,
        file: P<SourceFile>,
        position: u32,
    ) -> Option<CompletionList> {
        // We wanna walk up the tree till we find a JSX closing element.
        let jsx_closing_element = ast::find_ancestor_or_quit(location, |node| match node.kind() {
            Kind::JsxClosingElement => FindAncestorResult::True,
            Kind::LessThanSlashToken
            | Kind::GreaterThanToken
            | Kind::Identifier
            | Kind::PropertyAccessExpression => FindAncestorResult::False,
            _ => FindAncestorResult::Quit,
        })?;

        // In the TypeScript JSX element, if such element is not defined. When users query for completion at closing tag,
        // instead of simply giving unknown value, the completion will return the tag-name of an associated opening-element.
        // For example:
        //     var x = <div> </ /*1*/
        // The completion list at "1" will contain "div>" with type any
        // And at `<div> </ /*1*/ >` (with a closing `>`), the completion list will contain "div".
        // And at property access expressions `<MainComponent.Child> </MainComponent. /*1*/ >` the completion will
        // return full closing tag with an optional replacement span
        // For example:
        //     var x = <MainComponent.Child> </     MainComponent /*1*/  >
        //     var y = <MainComponent.Child> </   /*2*/   MainComponent >
        // the completion list at "1" and "2" will contain "MainComponent.Child" with a replacement span of closing tag name
        let has_closing_angle_bracket =
            astnav::find_child_of_kind(jsx_closing_element, Kind::GreaterThanToken, file).is_some();
        let tag_name =
            jsx_closing_element.parent().unwrap().as_jsx_element().opening_element.tag_name();
        let closing_tag = scanner::get_text_of_node(tag_name);
        let full_closing_tag = closing_tag + if has_closing_angle_bracket { "" } else { ">" };
        let (optional_replacement_span, fidelity) =
            self.create_lsp_range_from_node(jsx_closing_element.tag_name(), file);
        if !fidelity.is_exact() {
            return None;
        }
        let default_commit_characters =
            get_default_commit_characters(false /*isNewIdentifierLocation*/);

        let lsp_item = Self::create_lsp_completion_item(
            ctx,
            &full_closing_tag, /*name*/
            "",                /*insertText*/
            "",                /*filterText*/
            SORT_TEXT_LOCATION_PRIORITY,
            ScriptElementKind::ClassElement,
            ScriptElementKindModifier::None, /*kindModifiers*/
            None,                            /*replacementSpan*/
            None,                            /*commitCharacters*/
            None,                            /*labelDetails*/
            file,
            position,
            true,  /*isMemberCompletion*/
            false, /*isSnippet*/
            false, /*hasAction*/
            false, /*preselect*/
            "",    /*source*/
            None,  /*autoImportEntryData*/
            // !!! jsx autoimports
            None, /*additionalTextEdits*/
            None, /*detail*/
        );
        let item = CompletionItem::new(lsp_item);
        let mut items = vec![item];
        let item_defaults = self.set_item_defaults(
            ctx,
            position,
            file,
            &mut items,
            Some(&default_commit_characters),
            Some(optional_replacement_span),
        );

        Some(CompletionList { is_incomplete: false, item_defaults, items, ..Default::default() })
    }

    // completions.go:5023
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "The boolean parameters preserve the upstream Go function signature."
    )]
    pub(crate) fn create_lsp_completion_item(
        _ctx: &Context,
        name: &str,
        insert_text: &str,
        filter_text: &str,
        sort_text: &str,
        element_kind: ScriptElementKind,
        kind_modifiers: ScriptElementKindModifier,
        replacement_span: Option<lsproto::Range>,
        commit_characters: Option<Vec<String>>,
        label_details: Option<lsproto::CompletionItemLabelDetails>,
        file: P<SourceFile>,
        position: u32,
        is_member_completion: bool,
        is_snippet: bool,
        has_action: bool,
        preselect: bool,
        source: &str,
        auto_import_fix: Option<lsproto::AutoImportFix>,
        additional_text_edits: Option<Vec<lsproto::TextEdit>>,
        detail: Option<String>,
    ) -> lsproto::CompletionItem {
        let mut name = name.to_string();
        let mut insert_text = insert_text.to_string();
        let mut filter_text = filter_text.to_string();
        let kind = get_completions_symbol_kind(element_kind);
        let data = lsproto::CompletionItemData {
            file_name: file.original_file_name().to_string(),
            position: i32::try_from(position).expect("completion position exceeds protocol i32"),
            supplemental_file_index: supplemental_file_index(file).map(|index| {
                i32::try_from(index).expect("supplemental file index exceeds protocol i32")
            }),
            source: source.to_string(),
            name: name.clone(),
            auto_import: auto_import_fix,
            ..Default::default()
        };

        // Text edit
        let mut text_edit: Option<lsproto::TextEditOrInsertReplaceEdit> = None;
        if let Some(replacement_span) = replacement_span {
            text_edit = Some(lsproto::TextEditOrInsertReplaceEdit {
                text_edit: Some(lsproto::TextEdit {
                    new_text: if insert_text.is_empty() {
                        name.clone()
                    } else {
                        insert_text.clone()
                    },
                    range: replacement_span,
                }),
                ..Default::default()
            });
        }

        // Filter text

        // Ported from vscode ts extension.
        let (word_size, word_start) = get_word_length_and_start(file, position);
        let dot_accessor = get_dot_accessor(
            file,
            position.saturating_sub(u32::try_from(word_size).expect("word length exceeds u32")),
        );
        if filter_text.is_empty() {
            filter_text =
                get_filter_text(file, position, &insert_text, &name, word_start, &dot_accessor);
        }

        // Adjustements based on kind modifiers.
        let mut tags: Option<Vec<lsproto::CompletionItemTag>> = None;
        // Copied from vscode ts extension: `MyCompletionItem.constructor`.
        if is_member_completion && kind_modifiers.intersects(ScriptElementKindModifier::Optional) {
            if insert_text.is_empty() {
                insert_text.clone_from(&name);
            }
            if filter_text.is_empty() || is_snippet {
                filter_text.clone_from(&name);
            }
            name += "?";
        }
        if kind_modifiers.intersects(ScriptElementKindModifier::Deprecated) {
            tags = Some(vec![lsproto::CompletionItemTag::DEPRECATED]);
        }

        if has_action && !source.is_empty() {
            // !!! adjust label like vscode does
        }

        // Client assumes plain text by default.
        let mut insert_text_format: Option<lsproto::InsertTextFormat> = None;
        if is_snippet {
            insert_text_format = Some(lsproto::InsertTextFormat::SNIPPET);
        }

        lsproto::CompletionItem {
            label: name,
            label_details,
            kind: Some(kind),
            tags,
            detail,
            preselect: bool_to_ptr(preselect),
            sort_text: Some(sort_text.to_string()),
            filter_text: str_ptr_to(&filter_text),
            insert_text: str_ptr_to(&insert_text),
            insert_text_format,
            text_edit,
            commit_characters,
            additional_text_edits,
            data: Some(data),
            ..Default::default()
        }
    }

    // completions.go:5119
    pub(crate) fn get_label_completions_at_position(
        &self,
        ctx: &Context,
        node: P<Node>,
        file: P<SourceFile>,
        position: u32,
        optional_replacement_span: Option<lsproto::Range>,
    ) -> Option<CompletionList> {
        let mut items = Self::get_label_statement_completions(ctx, node, file, position);
        if items.is_empty() {
            return None;
        }
        let default_commit_characters =
            get_default_commit_characters(false /*isNewIdentifierLocation*/);
        let item_defaults = self.set_item_defaults(
            ctx,
            position,
            file,
            &mut items,
            Some(&default_commit_characters),
            optional_replacement_span,
        );
        Some(CompletionList { is_incomplete: false, item_defaults, items, ..Default::default() })
    }

    // completions.go:5146
    fn get_label_statement_completions(
        ctx: &Context,
        node: P<Node>,
        file: P<SourceFile>,
        position: u32,
    ) -> Vec<CompletionItem> {
        let mut uniques: FxHashSet<&'static str> = FxHashSet::default();
        let mut items: Vec<CompletionItem> = Vec::new();
        let mut current = Some(node);
        while let Some(c) = current {
            if ast::is_function_like(c) {
                break;
            }
            if ast::is_labeled_statement(c) {
                let name = c.label().unwrap().text();
                if uniques.insert(name) {
                    let lsp_item = Self::create_lsp_completion_item(
                        ctx,
                        name,
                        "", /*insertText*/
                        "", /*filterText*/
                        SORT_TEXT_LOCATION_PRIORITY,
                        ScriptElementKind::Label,
                        ScriptElementKindModifier::None, /*kindModifiers*/
                        None,                            /*replacementSpan*/
                        None,                            /*commitCharacters*/
                        None,                            /*labelDetails*/
                        file,
                        position,
                        false, /*isMemberCompletion*/
                        false, /*isSnippet*/
                        false, /*hasAction*/
                        false, /*preselect*/
                        "",    /*source*/
                        None,  /*autoImportEntryData*/
                        None,  /*additionalTextEdits*/
                        None,  /*detail*/
                    );
                    items.push(CompletionItem::new(lsp_item));
                }
            }
            current = c.parent();
        }
        items
    }
}

// completions.go:5195
pub(crate) fn is_completion_list_blocker(
    context_token: P<Node>,
    previous_token: Option<P<Node>>,
    location: P<Node>,
    file: P<SourceFile>,
    position: u32,
    type_checker: &mut Checker,
) -> bool {
    is_in_string_or_regular_expression_or_template_literal(context_token, position)
        || is_solely_identifier_definition_location(
            context_token,
            previous_token,
            file,
            position,
            type_checker,
        )
        || is_dot_of_numeric_literal(context_token, file)
        || is_in_jsx_text(context_token, location)
        || ast::is_big_int_literal(context_token)
}

// completions.go:5210
fn is_in_string_or_regular_expression_or_template_literal(
    context_token: P<Node>,
    position: u32,
) -> bool {
    // To be "in" one of these literals, the position has to be:
    //   1. entirely within the token text.
    //   2. at the end position of an unterminated token.
    //   3. at the end of a regular expression (due to trailing flags like '/foo/g').
    (ast::is_regular_expression_literal(context_token)
        || ast::is_string_text_containing_node(context_token))
        && context_token.loc().contains_exclusive(position.into())
        || position == context_token.end().expect_source("parsed tokens have source positions")
            && (ast::is_unterminated_literal(context_token)
                || ast::is_regular_expression_literal(context_token))
}

// true if we are certain that the currently edited location must define a new location; false otherwise.
// completions.go:5222
fn is_solely_identifier_definition_location(
    context_token: P<Node>,
    previous_token: Option<P<Node>>,
    file: P<SourceFile>,
    position: u32,
    type_checker: &mut Checker,
) -> bool {
    let parent = context_token.parent().unwrap();
    let containing_node_kind = parent.kind();
    match context_token.kind() {
        Kind::CommaToken => {
            return containing_node_kind == Kind::VariableDeclaration
                || is_variable_declaration_list_but_not_type_argument(context_token, file, type_checker)
                || containing_node_kind == Kind::VariableStatement
                || containing_node_kind == Kind::EnumDeclaration // enum a { foo, |
                || is_function_like_but_not_constructor(containing_node_kind)
                || containing_node_kind == Kind::InterfaceDeclaration // interface A<T, |
                || containing_node_kind == Kind::ArrayBindingPattern // var [x, y|
                || containing_node_kind == Kind::TypeAliasDeclaration // type Map, K, |
                // class A<T, |
                // var C = class D<T, |
                || (ast::is_class_like(parent) && parent.type_parameter_list().is_some() && parent.type_parameter_list().unwrap().end() >= context_token.pos());
        }
        Kind::DotToken // var [.|
        | Kind::OpenBracketToken => return containing_node_kind == Kind::ArrayBindingPattern, // var [x|
        Kind::ColonToken => return containing_node_kind == Kind::BindingElement,    // var {x :html|
        Kind::OpenParenToken => {
            return containing_node_kind == Kind::CatchClause
                || is_function_like_but_not_constructor(containing_node_kind);
        }
        Kind::OpenBraceToken => return containing_node_kind == Kind::EnumDeclaration, // enum a { |
        Kind::LessThanToken => {
            return containing_node_kind == Kind::ClassDeclaration // class A< |
                || containing_node_kind == Kind::ClassExpression // var C = class D< |
                || containing_node_kind == Kind::InterfaceDeclaration // interface A< |
                || containing_node_kind == Kind::TypeAliasDeclaration // type List< |
                || ast::is_function_like_kind(containing_node_kind);
        }
        Kind::StaticKeyword => {
            return containing_node_kind == Kind::PropertyDeclaration
                && !ast::is_class_like(parent.parent().unwrap());
        }
        Kind::DotDotDotToken => {
            return containing_node_kind == Kind::Parameter
                || (parent.parent().is_some_and(|pp| pp.kind() == Kind::ArrayBindingPattern)); // var [...z|
        }
        Kind::PublicKeyword | Kind::PrivateKeyword | Kind::ProtectedKeyword => {
            return containing_node_kind == Kind::Parameter
                && !ast::is_constructor_declaration(parent.parent().unwrap());
        }
        Kind::AsKeyword => {
            return containing_node_kind == Kind::ImportSpecifier
                || containing_node_kind == Kind::ExportSpecifier
                || containing_node_kind == Kind::NamespaceImport;
        }
        Kind::GetKeyword | Kind::SetKeyword => {
            return !is_from_object_type_declaration(context_token);
        }
        Kind::Identifier => {
            if (containing_node_kind == Kind::ImportSpecifier
                || containing_node_kind == Kind::ExportSpecifier)
                && Some(context_token) == parent.name()
                && context_token.text() == "type"
            {
                // import { type | }
                return false;
            }
            let ancestor_variable_declaration =
                ast::find_ancestor(parent, ast::is_variable_declaration);
            if ancestor_variable_declaration.is_some()
                && get_line_end_of_position(
                    file,
                    context_token.end().expect_source("parsed tokens have source positions"),
                ) < position
            {
                // let a
                // |
                return false;
            }
        }
        Kind::ClassKeyword
        | Kind::EnumKeyword
        | Kind::InterfaceKeyword
        | Kind::FunctionKeyword
        | Kind::VarKeyword
        | Kind::ImportKeyword
        | Kind::LetKeyword
        | Kind::ConstKeyword
        | Kind::InferKeyword => return true,
        Kind::TypeKeyword => {
            // import { type foo| }
            return containing_node_kind != Kind::ImportSpecifier;
        }
        Kind::AsteriskToken => {
            return ast::is_function_like(parent) && !ast::is_method_declaration(parent);
        }
        _ => {}
    }

    let token_kind = keyword_for_node(context_token);
    // If the previous token is keyword corresponding to class member completion keyword
    // there will be completion available here
    if is_class_member_completion_keyword(token_kind)
        && is_from_object_type_declaration(context_token)
    {
        return false;
    }

    if is_constructor_parameter_completion(context_token) {
        // constructor parameter completion is available only if
        // - its modifier of the constructor parameter or
        // - its name of the parameter and not being edited
        // eg. constructor(a |<- this shouldnt show completion
        if !ast::is_identifier(context_token)
            || ast::is_parameter_property_modifier(token_kind)
            || is_currently_editing_node(context_token, file, position)
        {
            return false;
        }
    }

    // Previous token may have been a keyword that was converted to an identifier.
    match keyword_for_node(context_token) {
        Kind::AbstractKeyword
        | Kind::ClassKeyword
        | Kind::DeclareKeyword
        | Kind::EnumKeyword
        | Kind::FunctionKeyword
        | Kind::InterfaceKeyword
        | Kind::LetKeyword
        | Kind::PrivateKeyword
        | Kind::ProtectedKeyword
        | Kind::PublicKeyword
        | Kind::StaticKeyword
        | Kind::VarKeyword => return true,
        Kind::AsyncKeyword => return ast::is_property_declaration(context_token.parent().unwrap()),
        _ => {}
    }

    // If we are inside a class declaration, and `constructor` is totally not present,
    // but we request a completion manually at a whitespace...
    let ancestor_class_like = ast::find_ancestor(parent, ast::is_class_like);
    if ancestor_class_like.is_some()
        && Some(context_token) == previous_token
        && is_previous_property_declaration_terminated(context_token, file, position)
    {
        // Don't block completions.
        return false;
    }

    let ancestor_property_declaration = ast::find_ancestor(parent, ast::is_property_declaration);
    // If we are inside a class declaration and typing `constructor` after property declaration...
    if let Some(ancestor_property_declaration) = ancestor_property_declaration {
        let previous_token = previous_token.unwrap();
        if Some(context_token) != Some(previous_token)
            && ast::is_class_like(previous_token.parent().unwrap().parent().unwrap())
            // And the cursor is at the token...
            && position <= previous_token.end().expect_source("parsed tokens have source positions")
        {
            // If we are sure that the previous property declaration is terminated according to newline or semicolon...
            if is_previous_property_declaration_terminated(
                context_token,
                file,
                previous_token.end().expect_source("parsed tokens have source positions"),
            ) {
                // Don't block completions.
                return false;
            } else if context_token.kind() != Kind::EqualsToken
                // Should not block: `class C { blah = c/**/ }`
                // But should block: `class C { blah = somewhat c/**/ }` and `class C { blah: SomeType c/**/ }`
                && (ast::is_initialized_property(ancestor_property_declaration) || ancestor_property_declaration.type_node().is_some())
            {
                return true;
            }
        }
    }
    if token_kind == Kind::ConstKeyword {
        return true;
    }
    ast::is_declaration_name(context_token)
        && !ast::is_shorthand_property_assignment(parent)
        && !ast::is_jsx_attribute(parent)
        // Don't block completions if we're in `class C /**/`, `interface I /**/` or `<T /**/>` ,
        // because we're *past* the end of the identifier and might want to complete `extends`.
        // If `contextToken !== previousToken`, this is `class C ex/**/`, `interface I ex/**/` or `<T ex/**/>`.
        && !((ast::is_class_like(parent) || ast::is_interface_declaration(parent) || ast::is_type_parameter_declaration(parent))
            && (Some(context_token) != previous_token
                || position > previous_token.unwrap().end().expect_source("parsed tokens have source positions")))
}

// completions.go:5366
fn is_variable_declaration_list_but_not_type_argument(
    node: P<Node>,
    file: P<SourceFile>,
    type_checker: &mut Checker,
) -> bool {
    node.parent().unwrap().kind() == Kind::VariableDeclarationList
        && !is_possibly_type_argument_position(Some(node), file, type_checker)
}

// completions.go:5371
fn is_function_like_but_not_constructor(kind: Kind) -> bool {
    ast::is_function_like_kind(kind) && kind != Kind::Constructor
}

// completions.go:5375
fn is_previous_property_declaration_terminated(
    context_token: P<Node>,
    file: P<SourceFile>,
    position: u32,
) -> bool {
    context_token.kind() != Kind::EqualsToken
        && (context_token.kind() == Kind::SemicolonToken
            || get_line_of_position(
                file,
                context_token.end().expect_source("parsed tokens have source positions"),
            ) != get_line_of_position(file, position))
}

// completions.go:5381
fn is_dot_of_numeric_literal(context_token: P<Node>, file: P<SourceFile>) -> bool {
    if context_token.kind() == Kind::NumericLiteral {
        let text = &file.text()[context_token
            .pos()
            .expect_source("parsed tokens have source positions")
            as usize
            ..context_token.end().expect_source("parsed tokens have source positions") as usize];
        let r = text.chars().next_back().unwrap_or('\u{FFFD}');
        return r == '.';
    }

    false
}

// completions.go:5391
fn is_in_jsx_text(context_token: P<Node>, location: P<Node>) -> bool {
    if context_token.kind() == Kind::JsxText {
        return true;
    }

    if context_token.kind() == Kind::GreaterThanToken
        && let Some(ct_parent) = context_token.parent()
    {
        // <Component<string> /**/ />
        // <Component<string> /**/ ><Component>
        // - contextToken: GreaterThanToken (before cursor)
        // - location: JsxSelfClosingElement or JsxOpeningElement
        // - contextToken.parent === location
        if location == ct_parent && ast::is_jsx_opening_like_element(location) {
            return false;
        }

        if ct_parent.kind() == Kind::JsxOpeningElement {
            // <div>/**/
            // - contextToken: GreaterThanToken (before cursor)
            // - location: JSXElement
            // - different parents (JSXOpeningElement, JSXElement)
            return location.parent().unwrap().kind() != Kind::JsxOpeningElement;
        }

        if ct_parent.kind() == Kind::JsxClosingElement
            || ct_parent.kind() == Kind::JsxSelfClosingElement
        {
            return ct_parent.parent().is_some_and(|p| p.kind() == Kind::JsxElement);
        }
    }

    false
}

// completions.go:5423
pub(crate) fn client_supports_item_label_details(ctx: &Context) -> bool {
    lsproto::get_client_capabilities(ctx)
        .text_document
        .completion
        .completion_item
        .label_details_support
}

// completions.go:5427
pub(crate) fn client_supports_item_snippet(ctx: &Context) -> bool {
    lsproto::get_client_capabilities(ctx).text_document.completion.completion_item.snippet_support
}

// completions.go:5431
pub(crate) fn client_supports_item_commit_characters(ctx: &Context) -> bool {
    lsproto::get_client_capabilities(ctx)
        .text_document
        .completion
        .completion_item
        .commit_characters_support
}

// completions.go:5435
pub(crate) fn client_supports_item_insert_replace(ctx: &Context) -> bool {
    lsproto::get_client_capabilities(ctx)
        .text_document
        .completion
        .completion_item
        .insert_replace_support
}

// completions.go:5439
pub(crate) fn client_supports_default_commit_characters(ctx: &Context) -> bool {
    lsproto::get_client_capabilities(ctx)
        .text_document
        .completion
        .completion_list
        .item_defaults
        .iter()
        .any(|s| s == "commitCharacters")
}

// completions.go:5443
fn client_supports_default_edit_range(ctx: &Context) -> bool {
    lsproto::get_client_capabilities(ctx)
        .text_document
        .completion
        .completion_list
        .item_defaults
        .iter()
        .any(|s| s == "editRange")
}

// completions.go:5447
#[derive(Clone, Copy)]
pub struct ArgumentInfoForCompletions {
    pub(crate) invocation: P<Node>,
    pub(crate) argument_index: usize,
    pub(crate) argument_count: usize,
}

// completions.go:5453
pub(crate) fn get_argument_info_for_completions(
    node: P<Node>,
    position: u32,
    file: P<SourceFile>,
    type_checker: &mut Checker,
) -> Option<ArgumentInfoForCompletions> {
    let info = get_immediately_containing_argument_info(node, position, file, type_checker)?;
    if info.is_type_parameter_list || info.invocation.call_invocation.is_none() {
        return None;
    }
    Some(ArgumentInfoForCompletions {
        invocation: info.invocation.call_invocation.unwrap().node,
        argument_index: info.argument_index,
        argument_count: info.argument_count,
    })
}

// Case completions for switch statements
pub(crate) const SOURCE_SWITCH_CASES: &str = "SwitchCases/";
