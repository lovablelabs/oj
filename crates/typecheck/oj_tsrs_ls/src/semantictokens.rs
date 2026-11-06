// Port of ls/semantictokens.go.

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{
    self as ast, Kind, ModifierFlags, Node, NodeFlags, SemanticMeaning, SourceFile, Symbol,
    SymbolFlags,
};
use tsrs_checker::{Checker, SignatureKind, Type, TypeFlags};
use tsrs_compiler::Program;
use tsrs_core::context::Context;
use tsrs_core::goslices;
use tsrs_core::{P, TextRange};
use tsrs_lsproto as lsproto;
use tsrs_lsproto::{SemanticTokenModifier, SemanticTokenType};
use tsrs_scanner as scanner;

use crate::languageservice::LanguageService;
use crate::lsconv::Converters;
use crate::spanmap::{Feature, Fidelity};
use crate::utilities::get_meaning_from_location;

// tokenTypes defines the order of token types for encoding
// semantictokens.go:21
const TOKEN_TYPES: [SemanticTokenType; 23] = [
    SemanticTokenType::NAMESPACE,
    SemanticTokenType::CLASS,
    SemanticTokenType::ENUM,
    SemanticTokenType::INTERFACE,
    SemanticTokenType::STRUCT,
    SemanticTokenType::TYPE_PARAMETER,
    SemanticTokenType::TYPE,
    SemanticTokenType::PARAMETER,
    SemanticTokenType::VARIABLE,
    SemanticTokenType::PROPERTY,
    SemanticTokenType::ENUM_MEMBER,
    SemanticTokenType::DECORATOR,
    SemanticTokenType::EVENT,
    SemanticTokenType::FUNCTION,
    SemanticTokenType::METHOD,
    SemanticTokenType::MACRO,
    SemanticTokenType::LABEL,
    SemanticTokenType::COMMENT,
    SemanticTokenType::STRING,
    SemanticTokenType::KEYWORD,
    SemanticTokenType::NUMBER,
    SemanticTokenType::REGEXP,
    SemanticTokenType::OPERATOR,
];

// tokenModifiers defines the order of token modifiers for encoding
// semantictokens.go:48
const TOKEN_MODIFIERS: [SemanticTokenModifier; 11] = [
    SemanticTokenModifier::DECLARATION,
    SemanticTokenModifier::DEFINITION,
    SemanticTokenModifier::READONLY,
    SemanticTokenModifier::STATIC,
    SemanticTokenModifier::DEPRECATED,
    SemanticTokenModifier::ABSTRACT,
    SemanticTokenModifier::ASYNC,
    SemanticTokenModifier::MODIFICATION,
    SemanticTokenModifier::DOCUMENTATION,
    SemanticTokenModifier::DEFAULT_LIBRARY,
    SemanticTokenModifier("local"),
];

// semantictokens.go:62 (only the values this file produces; the others are positions in TOKEN_TYPES)
type TokenType = u32;

const TOKEN_TYPE_NAMESPACE: TokenType = 0;
const TOKEN_TYPE_CLASS: TokenType = 1;
const TOKEN_TYPE_ENUM: TokenType = 2;
const TOKEN_TYPE_INTERFACE: TokenType = 3;
const TOKEN_TYPE_TYPE_PARAMETER: TokenType = 5;
const TOKEN_TYPE_TYPE: TokenType = 6;
const TOKEN_TYPE_PARAMETER: TokenType = 7;
const TOKEN_TYPE_VARIABLE: TokenType = 8;
const TOKEN_TYPE_PROPERTY: TokenType = 9;
const TOKEN_TYPE_ENUM_MEMBER: TokenType = 10;
const TOKEN_TYPE_FUNCTION: TokenType = 13;
const TOKEN_TYPE_METHOD: TokenType = 14; // Previously called "member" in TypeScript

// semantictokens.go:90 (bit i is TOKEN_MODIFIERS[i]; only the bits this file sets are named)
type TokenModifier = u32;

const TOKEN_MODIFIER_DECLARATION: TokenModifier = 1 << 0;
const TOKEN_MODIFIER_READONLY: TokenModifier = 1 << 2;
const TOKEN_MODIFIER_STATIC: TokenModifier = 1 << 3;
const TOKEN_MODIFIER_ASYNC: TokenModifier = 1 << 6;
const TOKEN_MODIFIER_DEFAULT_LIBRARY: TokenModifier = 1 << 9;
const TOKEN_MODIFIER_LOCAL: TokenModifier = 1 << 10;

// SemanticTokensLegend returns the legend describing the token types and modifiers.
// It filters the legend to only include types and modifiers that the client supports,
// as indicated by clientCapabilities.
// semantictokens.go:109
pub fn semantic_tokens_legend(
    client_capabilities: &lsproto::ResolvedSemanticTokensClientCapabilities,
) -> lsproto::SemanticTokensLegend {
    let mut types = Vec::with_capacity(TOKEN_TYPES.len());
    types.extend(TOKEN_TYPES.into_iter().filter_map(|t| {
        if client_capabilities.token_types.iter().any(|c| c == t.0) {
            Some(t.0.to_string())
        } else {
            None
        }
    }));
    let mut modifiers = Vec::with_capacity(TOKEN_MODIFIERS.len());
    modifiers.extend(TOKEN_MODIFIERS.into_iter().filter_map(|m| {
        if client_capabilities.token_modifiers.iter().any(|c| c == m.0) {
            Some(m.0.to_string())
        } else {
            None
        }
    }));
    lsproto::SemanticTokensLegend { token_types: types, token_modifiers: modifiers }
}

impl LanguageService {
    // semantictokens.go:128
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    pub fn provide_semantic_tokens(
        &self,
        ctx: &Context,
        document_uri: &lsproto::DocumentUri,
    ) -> Result<lsproto::SemanticTokensResponse, lsproto::Error> {
        let (program, file) = self.get_program_and_file(document_uri);

        let supplemental = file.supplemental_source_files();
        let mut files = Vec::with_capacity(1 + supplemental.len());
        files.push(file);
        files.extend_from_slice(supplemental);
        let mut tokens: Vec<SemanticToken> = Vec::with_capacity(files.len());
        for projection in files {
            let mut c = program.get_type_checker_for_file(ctx, projection);
            tokens.extend(
                Self::collect_semantic_tokens(ctx, &mut c, projection, program).into_iter().map(
                    |mut token| {
                        token.file = Some(projection);
                        token
                    },
                ),
            );
        }
        sort_semantic_tokens(&mut tokens, &self.converters);

        if tokens.is_empty() {
            return Ok(lsproto::SemanticTokensOrNull::default());
        }

        // Convert to LSP format (relative encoding)
        let encoded = encode_semantic_tokens(ctx, &tokens, &self.converters);

        Ok(lsproto::SemanticTokensOrNull {
            semantic_tokens: Some(lsproto::SemanticTokens { data: encoded, ..Default::default() }),
        })
    }

    // semantictokens.go:160
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    pub fn provide_semantic_tokens_range(
        &self,
        ctx: &Context,
        document_uri: &lsproto::DocumentUri,
        rng: lsproto::Range,
    ) -> Result<lsproto::SemanticTokensRangeResponse, lsproto::Error> {
        let (program, file) = self.get_program_and_file(document_uri);

        let mapped_ranges = self.converters.from_lsp_range_intersecting_for_source_file(
            file,
            rng,
            Feature::SemanticTokens,
        );
        let mut tokens: Vec<SemanticToken> = Vec::with_capacity(mapped_ranges.len());
        let mut seen: FxHashSet<SemanticToken> = FxHashSet::default();
        for mapped in &mapped_ranges {
            let projection = mapped.script;
            let mut c = program.get_type_checker_for_file(ctx, projection);
            tokens.extend(
                Self::collect_semantic_tokens_in_range(
                    ctx,
                    &mut c,
                    projection,
                    program,
                    mapped
                        .span
                        .pos()
                        .expect_source("mapped semantic-token ranges have source positions"),
                    mapped
                        .span
                        .end()
                        .expect_source("mapped semantic-token ranges have source positions"),
                )
                .into_iter()
                .filter_map(|mut token| {
                    token.file = Some(projection);
                    if seen.insert(token) { Some(token) } else { None }
                }),
            );
        }
        sort_semantic_tokens(&mut tokens, &self.converters);

        if tokens.is_empty() {
            return Ok(lsproto::SemanticTokensOrNull::default());
        }

        // Convert to LSP format (relative encoding)
        let encoded = encode_semantic_tokens(ctx, &tokens, &self.converters);

        Ok(lsproto::SemanticTokensOrNull {
            semantic_tokens: Some(lsproto::SemanticTokens { data: encoded, ..Default::default() }),
        })
    }
}

fn go_cmp_compare<T: Ord + Copy>(a: T, b: T) -> std::cmp::Ordering {
    a.cmp(&b)
}

// semantictokens.go:193
fn sort_semantic_tokens(tokens: &mut [SemanticToken], converters: &Converters) {
    // tsrs-only: each token's start is converted once, not in every comparison (Go converts two per comparison).
    // The comparisons are the same, so the order is too; a conversion looks up the file's line map in the snapshot,
    // and concurrent requests contended on those lookups.
    let mut keyed: Vec<(lsproto::Position, SemanticToken)> = tokens
        .iter()
        .map(|&token| (semantic_token_lsp_range(&token, converters).0.start, token))
        .collect();
    goslices::sort_func(&mut keyed, |(a_start, a), (b_start, b)| {
        let result = go_cmp_compare(a_start.line, b_start.line);
        if !result.is_eq() {
            return result;
        }
        let result = go_cmp_compare(a_start.character, b_start.character);
        if !result.is_eq() {
            return result;
        }
        let result =
            go_cmp_compare(a.file.unwrap().path().as_str(), b.file.unwrap().path().as_str());
        if !result.is_eq() {
            return result;
        }
        go_cmp_compare(a.node.pos(), b.node.pos())
    });
    for (slot, (_, token)) in tokens.iter_mut().zip(keyed) {
        *slot = token;
    }
}

// semantictokens.go:210
fn semantic_token_lsp_range(
    token: &SemanticToken,
    converters: &Converters,
) -> (lsproto::Range, Fidelity) {
    let file = token.file.unwrap();
    let start = scanner::get_token_pos_of_node(token.node, file, false);
    converters.to_lsp_range_for_feature(
        &file,
        TextRange::new(
            start,
            token.node.end().expect_source("semantic-token nodes have source positions"),
        ),
        Feature::SemanticTokens,
    )
}

// semantictokens.go:215
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct SemanticToken {
    node: P<Node>,
    file: Option<P<SourceFile>>,
    token_type: TokenType,
    token_modifier: TokenModifier,
}

impl LanguageService {
    // semantictokens.go:222
    fn collect_semantic_tokens(
        ctx: &Context,
        c: &mut Checker,
        file: P<SourceFile>,
        program: &Program,
    ) -> Vec<SemanticToken> {
        Self::collect_semantic_tokens_in_range(
            ctx,
            c,
            file,
            program,
            file.as_node().pos().expect_source("parsed source files have source positions"),
            file.as_node().end().expect_source("parsed source files have source positions"),
        )
    }

    // semantictokens.go:226
    fn collect_semantic_tokens_in_range(
        ctx: &Context,
        c: &mut Checker,
        file: P<SourceFile>,
        program: &Program,
        span_start: u32,
        span_end: u32,
    ) -> Vec<SemanticToken> {
        let mut v = SemanticTokensVisitor {
            ctx,
            c,
            file,
            program,
            span_start,
            span_end,
            tokens: Vec::new(),
            in_jsx_element: false,
        };

        v.visit(file.as_node());
        let tokens = v.tokens;

        // Check for cancellation after collection
        if ctx.err().is_some() {
            return Vec::new();
        }

        tokens
    }
}

// The `visit` closure of collectSemanticTokensInRange and the state it captures.
struct SemanticTokensVisitor<'a> {
    ctx: &'a Context,
    c: &'a mut Checker,
    file: P<SourceFile>,
    program: &'a Program,
    span_start: u32,
    span_end: u32,
    tokens: Vec<SemanticToken>,
    in_jsx_element: bool,
}

impl SemanticTokensVisitor<'_> {
    fn visit(&mut self, node: P<Node>) -> bool {
        // Check for cancellation
        if self.ctx.err().is_some() {
            return false;
        }

        if node.flags().intersects(NodeFlags::Reparsed) {
            return false;
        }
        let node_end = node.end().expect_source("semantic-token nodes have source positions");
        if node.pos().expect_source("semantic-token nodes have source positions") >= self.span_end
            || node_end <= self.span_start
        {
            return false;
        }

        let prev_in_jsx_element = self.in_jsx_element;
        if ast::is_jsx_element(node) || ast::is_jsx_self_closing_element(node) {
            self.in_jsx_element = true;
        } else if ast::is_jsx_expression(node) {
            self.in_jsx_element = false;
        }

        if (ast::is_identifier(node) || ast::is_private_identifier(node))
            && !node.text().is_empty()
            && !self.in_jsx_element
            && !is_in_import_clause(node)
            && !is_infinity_or_nan_string(node.text())
        {
            self.add_token_for_identifier(node);
        }

        node.for_each_child(&mut |child| self.visit(child));
        self.in_jsx_element = prev_in_jsx_element;
        false
    }

    fn add_token_for_identifier(&mut self, node: P<Node>) {
        let c = &mut *self.c;
        let Some(mut symbol) = c.get_symbol_at_location_exported(node) else {
            return;
        };
        // Resolve aliases
        if symbol.flags().intersects(SymbolFlags::Alias) {
            symbol = c.get_aliased_symbol(symbol);
        }

        let Some(mut token_type) = classify_symbol(symbol, get_meaning_from_location(node)) else {
            return;
        };
        let mut token_modifier: TokenModifier = 0;

        // Check if this is a declaration
        if let Some(parent) = node.parent() {
            let parent_is_declaration = ast::is_binding_element(parent)
                || token_from_declaration_mapping(parent.kind()) == Some(token_type);
            if parent_is_declaration && parent.name() == Some(node) {
                token_modifier |= TOKEN_MODIFIER_DECLARATION;
            }
        }

        // Property declaration in constructor: reclassify parameters as properties in property access context
        if token_type == TOKEN_TYPE_PARAMETER
            && ast::is_right_side_of_qualified_name_or_property_access(node)
        {
            token_type = TOKEN_TYPE_PROPERTY;
        }

        // Type-based reclassification
        token_type = reclassify_by_type(c, node, token_type);

        // Get the value declaration to check modifiers
        if let Some(decl) = symbol.value_declaration() {
            let modifiers = ast::get_combined_modifier_flags(decl);
            let node_flags = ast::get_combined_node_flags(decl);

            if modifiers.intersects(ModifierFlags::Static) {
                token_modifier |= TOKEN_MODIFIER_STATIC;
            }
            if modifiers.intersects(ModifierFlags::Async) {
                token_modifier |= TOKEN_MODIFIER_ASYNC;
            }
            if token_type != TOKEN_TYPE_CLASS
                && token_type != TOKEN_TYPE_INTERFACE
                && (modifiers.intersects(ModifierFlags::Readonly)
                    || node_flags.intersects(NodeFlags::Const)
                    || symbol.flags().intersects(SymbolFlags::EnumMember))
            {
                token_modifier |= TOKEN_MODIFIER_READONLY;
            }
            if (token_type == TOKEN_TYPE_VARIABLE || token_type == TOKEN_TYPE_FUNCTION)
                && is_local_declaration(decl, self.file)
            {
                token_modifier |= TOKEN_MODIFIER_LOCAL;
            }
            if let Some(decl_source_file) = ast::get_source_file_of_node(decl)
                && self.program.is_source_file_default_library(decl_source_file.path())
            {
                token_modifier |= TOKEN_MODIFIER_DEFAULT_LIBRARY;
            }
        } else if !symbol.declarations().is_empty() {
            for &decl in symbol.declarations() {
                if let Some(decl_source_file) = ast::get_source_file_of_node(decl)
                    && self.program.is_source_file_default_library(decl_source_file.path())
                {
                    token_modifier |= TOKEN_MODIFIER_DEFAULT_LIBRARY;
                    break;
                }
            }
        }

        self.tokens.push(SemanticToken { node, file: None, token_type, token_modifier });
    }
}

// semantictokens.go:342
fn classify_symbol(symbol: P<Symbol>, meaning: SemanticMeaning) -> Option<TokenType> {
    let flags = symbol.flags();
    if flags.intersects(SymbolFlags::Class) {
        return Some(TOKEN_TYPE_CLASS);
    }
    if flags.intersects(SymbolFlags::Enum) {
        return Some(TOKEN_TYPE_ENUM);
    }
    if flags.intersects(SymbolFlags::TypeAlias) {
        return Some(TOKEN_TYPE_TYPE);
    }
    if flags.intersects(SymbolFlags::Interface) && meaning.intersects(SemanticMeaning::Type) {
        return Some(TOKEN_TYPE_INTERFACE);
    }
    if flags.intersects(SymbolFlags::TypeParameter) {
        return Some(TOKEN_TYPE_TYPE_PARAMETER);
    }

    // Check the value declaration
    let mut decl = symbol.value_declaration();
    if decl.is_none() && !symbol.declarations().is_empty() {
        decl = Some(symbol.declarations()[0]);
    }
    if let Some(mut decl) = decl {
        if ast::is_binding_element(decl) {
            decl = get_declaration_for_binding_element(decl);
        }
        if let Some(token_type) = token_from_declaration_mapping(decl.kind()) {
            return Some(token_type);
        }
    }

    None
}

// semantictokens.go:379
fn token_from_declaration_mapping(kind: Kind) -> Option<TokenType> {
    Some(match kind {
        Kind::VariableDeclaration => TOKEN_TYPE_VARIABLE,
        Kind::Parameter => TOKEN_TYPE_PARAMETER,
        Kind::PropertyDeclaration
        | Kind::GetAccessor
        | Kind::SetAccessor
        | Kind::PropertySignature
        | Kind::PropertyAssignment
        | Kind::ShorthandPropertyAssignment => TOKEN_TYPE_PROPERTY,
        Kind::ModuleDeclaration => TOKEN_TYPE_NAMESPACE,
        Kind::EnumDeclaration => TOKEN_TYPE_ENUM,
        Kind::EnumMember => TOKEN_TYPE_ENUM_MEMBER,
        Kind::ClassDeclaration | Kind::ClassExpression => TOKEN_TYPE_CLASS,
        Kind::MethodDeclaration | Kind::MethodSignature => TOKEN_TYPE_METHOD,
        Kind::FunctionDeclaration | Kind::FunctionExpression => TOKEN_TYPE_FUNCTION,
        Kind::InterfaceDeclaration => TOKEN_TYPE_INTERFACE,
        Kind::TypeAliasDeclaration => TOKEN_TYPE_TYPE,
        Kind::TypeParameter => TOKEN_TYPE_TYPE_PARAMETER,
        _ => return None,
    })
}

// semantictokens.go:418
fn reclassify_by_type(c: &mut Checker, node: P<Node>, tt: TokenType) -> TokenType {
    // Type-based reclassification for variables, properties, and parameters
    if tt == TOKEN_TYPE_VARIABLE || tt == TOKEN_TYPE_PROPERTY || tt == TOKEN_TYPE_PARAMETER {
        let typ = c.get_type_at_location(node);
        let test = |c: &mut Checker, condition: &dyn Fn(&mut Checker, P<Type>) -> bool| -> bool {
            if condition(c, typ) {
                return true;
            }
            if typ.flags().intersects(TypeFlags::Union)
                && typ.as_union_type().types().iter().any(|&t| condition(c, t))
            {
                return true;
            }
            false
        };

        // Check for constructor signatures (class-like)
        if tt != TOKEN_TYPE_PARAMETER
            && test(c, &|c, t| !c.get_signatures_of_type(t, SignatureKind::Construct).is_empty())
        {
            return TOKEN_TYPE_CLASS;
        }

        // Check for call signatures (function-like)
        // Must have call signatures AND (no properties OR be used in call context)
        let has_call_signatures =
            test(c, &|c, t| !c.get_signatures_of_type(t, SignatureKind::Call).is_empty());
        if has_call_signatures {
            // Go reads `objType.Properties()`, resolved by the signature queries above; with lazy member tables they
            // may have been answered without resolving.
            let has_no_properties = !test(c, &|c, t| {
                t.data().as_object_type().is_some()
                    && c.has_properties_of_structured_type_exported(t)
            });
            if has_no_properties || is_expression_in_call_expression(node) {
                if tt == TOKEN_TYPE_PROPERTY {
                    return TOKEN_TYPE_METHOD;
                }
                return TOKEN_TYPE_FUNCTION;
            }
        }
    }
    tt
}

// semantictokens.go:464
fn is_local_declaration(decl: P<Node>, source_file: P<SourceFile>) -> bool {
    let mut decl = decl;
    if ast::is_binding_element(decl) {
        decl = get_declaration_for_binding_element(decl);
    }
    if ast::is_variable_declaration(decl) {
        let parent = decl.parent();
        // Check if this is a catch clause parameter
        if let Some(parent) = parent {
            if ast::is_catch_clause(parent) {
                return ast::get_source_file_of_node(decl) == Some(source_file);
            }
            if ast::is_variable_declaration_list(parent)
                && let Some(grandparent) = parent.parent()
            {
                let great_grandparent = grandparent.parent();
                return (!great_grandparent.is_some_and(ast::is_source_file)
                    || ast::is_catch_clause(grandparent))
                    && ast::get_source_file_of_node(decl) == Some(source_file);
            }
        }
    } else if ast::is_function_declaration(decl) {
        let parent = decl.parent();
        return parent.is_some_and(|p| !ast::is_source_file(p))
            && ast::get_source_file_of_node(decl) == Some(source_file);
    }
    false
}

// semantictokens.go:489
fn get_declaration_for_binding_element(element: P<Node>) -> P<Node> {
    let mut element = element;
    loop {
        if let Some(parent) = element.parent().filter(|p| ast::is_binding_pattern(*p)) {
            if let Some(grandparent) = parent.parent().filter(|g| ast::is_binding_element(*g)) {
                element = grandparent;
                continue;
            }
            // Go returns parent.Parent, which may be nil (dereferenced by the callers).
            return parent.parent().unwrap();
        }
        return element;
    }
}

// semantictokens.go:504
fn is_in_import_clause(node: P<Node>) -> bool {
    node.parent().is_some_and(|parent| {
        ast::is_import_clause(parent)
            || ast::is_import_specifier(parent)
            || ast::is_namespace_import(parent)
    })
}

// semantictokens.go:509
fn is_expression_in_call_expression(node: P<Node>) -> bool {
    let mut node = node;
    while ast::is_right_side_of_qualified_name_or_property_access(node) {
        node = node.parent().unwrap();
    }
    node.parent()
        .is_some_and(|parent| ast::is_call_expression(parent) && parent.expression() == Some(node))
}

// semantictokens.go:517
fn is_infinity_or_nan_string(text: &str) -> bool {
    text == "Infinity" || text == "NaN"
}

// encodeSemanticTokens encodes tokens into the LSP format using relative positioning.
// It filters tokens based on client capabilities, only including types and modifiers that the client supports.
// semantictokens.go:523
fn encode_semantic_tokens(
    ctx: &Context,
    tokens: &[SemanticToken],
    converters: &Converters,
) -> Vec<u32> {
    // Build mapping from server token types/modifiers to client indices
    let mut type_mapping: FxHashMap<TokenType, u32> = FxHashMap::default();
    let mut modifier_mapping: FxHashMap<SemanticTokenModifier, u32> = FxHashMap::default();

    let caps = lsproto::get_client_capabilities(ctx);
    let client_capabilities = &caps.text_document.semantic_tokens;

    // Map server token types to client-supported indices
    type_mapping.extend(
        TOKEN_TYPES
            .iter()
            .enumerate()
            .filter(|(_, server_type)| {
                client_capabilities.token_types.iter().any(|c| c == server_type.0)
            })
            .enumerate()
            .map(|(client_idx, (i, _))| (i as TokenType, client_idx as u32)),
    );

    // Map server token modifiers to client-supported bit positions
    modifier_mapping.extend(
        TOKEN_MODIFIERS
            .into_iter()
            .filter(|server_modifier| {
                client_capabilities.token_modifiers.iter().any(|c| c == server_modifier.0)
            })
            .enumerate()
            .map(|(client_bit, server_modifier)| (server_modifier, client_bit as u32)),
    );

    // Each token encodes 5 uint32 values: deltaLine, deltaChar, length, tokenType, tokenModifiers
    let mut encoded: Vec<u32> = Vec::with_capacity(tokens.len() * 5);
    let mut prev_line: u32 = 0;
    let mut prev_char: u32 = 0;

    for token in tokens {
        // Skip tokens with types not supported by the client
        let Some(&client_type_idx) = type_mapping.get(&token.token_type) else {
            continue;
        };

        // Map modifiers to client-supported bit mask
        let mut client_modifier_mask: u32 = 0;
        client_modifier_mask = TOKEN_MODIFIERS.iter().enumerate().fold(
            client_modifier_mask,
            |mut client_modifier_mask, (i, server_modifier)| {
                if token.token_modifier & (1 << i) != 0
                    && let Some(&client_bit) = modifier_mapping.get(server_modifier)
                {
                    client_modifier_mask |= 1 << client_bit;
                }

                client_modifier_mask
            },
        );

        // Semantic tokens must describe one concrete source segment; synthesized and cross-segment
        // tokens do not identify a coherent token in the original text.
        let (lsp_range, fidelity) = semantic_token_lsp_range(token, converters);
        if !fidelity.is_exact() {
            continue;
        }
        let start_pos = lsp_range.start;
        let end_pos = lsp_range.end;

        // Length is the character difference when on the same line
        let token_length: u32 = if start_pos.line == end_pos.line {
            end_pos.character - start_pos.character
        } else {
            panic!(
                "semantic tokens: token spans multiple lines: start=({},{}) end=({},{}) for token at offset {}",
                start_pos.line,
                start_pos.character,
                end_pos.line,
                end_pos.character,
                token.node.pos().expect_source("semantic-token nodes have source positions")
            );
        };

        let line = start_pos.line;
        let char = start_pos.character;

        // Multiple virtual projections can describe the same original token; LSP requires one entry per
        // start position, so retain the first after sorting.
        if !encoded.is_empty() && line == prev_line && char == prev_char {
            continue;
        }
        assert!(
            !(!encoded.is_empty() && (line < prev_line || line == prev_line && char < prev_char)),
            "semantic tokens: positions must be strictly increasing: prev=({},{}) current=({},{}) for token at offset {}",
            prev_line,
            prev_char,
            line,
            char,
            token.node.pos().expect_source("semantic-token nodes have source positions")
        );

        // Encode as: [deltaLine, deltaChar, length, tokenType, tokenModifiers]
        let delta_line = line - prev_line;
        let delta_char = if delta_line == 0 { char - prev_char } else { char };

        encoded.extend_from_slice(&[
            delta_line,
            delta_char,
            token_length,
            client_type_idx,
            client_modifier_mask,
        ]);

        prev_line = line;
        prev_char = char;
    }

    encoded
}
