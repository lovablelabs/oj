use tsrs_ast::{self as ast, FindAncestorResult, Kind, Node};
use tsrs_core::{LanguageVariant, P, TextChange, TextRange};
use tsrs_scanner::Scanner;

use super::*;

// scanner.go:12
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TextRangeWithKind {
    pub(crate) loc: TextRange,
    pub(crate) kind: Kind,
}

// scanner.go:17
pub(crate) fn new_text_range_with_kind(pos: u32, end: u32, kind: Kind) -> TextRangeWithKind {
    TextRangeWithKind { loc: TextRange::new(pos, end), kind }
}

// scanner.go:24
#[derive(Clone, Default)]
pub struct TokenInfo {
    pub(crate) leading_trivia: Vec<TextRangeWithKind>,
    pub(crate) token: TextRangeWithKind,
    pub(crate) trailing_trivia: Vec<TextRangeWithKind>,
}

// scanner.go:30
pub(crate) struct FormattingScanner {
    s: Scanner,
    start_pos: u32,
    end_pos: u32,
    saved_pos: u32,
    has_last_token_info: bool,
    last_token_info: TokenInfo,
    last_scan_action: ScanAction,
    leading_trivia: Vec<TextRangeWithKind>,
    trailing_trivia: Vec<TextRangeWithKind>,
    was_new_line: bool,
}

// scanner.go:43
pub(crate) fn new_formatting_scanner(
    text: &'static str,
    language_variant: LanguageVariant,
    start_pos: u32,
    end_pos: u32,
    worker: &mut FormatSpanWorker,
) -> Vec<TextChange> {
    let mut scan = Scanner::new();
    scan.set_skip_trivia(false);
    scan.set_language_variant(language_variant);
    scan.set_text(text);
    scan.reset_token_state(start_pos);

    let fmt_scn = FormattingScanner {
        s: scan,
        start_pos,
        end_pos,
        saved_pos: 0,
        has_last_token_info: false,
        last_token_info: TokenInfo::default(),
        last_scan_action: ScanAction::Scan,
        leading_trivia: Vec::new(),
        trailing_trivia: Vec::new(),
        was_new_line: true,
    };

    let res = worker.execute(fmt_scn);

    if let Some(fmt_scn) = worker.formatting_scanner.as_mut() {
        fmt_scn.has_last_token_info = false;
        fmt_scn.s.reset();
    }

    res
}

impl FormattingScanner {
    // scanner.go:65
    pub(crate) fn advance(&mut self) {
        self.has_last_token_info = false;
        let is_started = self.s.token_full_start() != self.start_pos;

        if is_started {
            self.was_new_line =
                self.trailing_trivia.last().is_some_and(|t| t.kind == Kind::NewLineTrivia);
        } else {
            self.s.scan();
        }

        self.leading_trivia = Vec::new();
        self.trailing_trivia = Vec::new();

        let mut pos = self.s.token_full_start();

        // Read leading trivia and token
        while pos < self.end_pos {
            let t = self.s.token();
            if !ast::is_trivia(t) {
                break;
            }

            // consume leading trivia
            self.s.scan();
            let item = new_text_range_with_kind(pos, self.s.token_full_start(), t);

            pos = self.s.token_full_start();

            self.leading_trivia.push(item);
        }

        self.saved_pos = self.s.token_full_start();
    }
}

// scanner.go:99
fn should_rescan_greater_than_token(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::GreaterThanEqualsToken
            | Kind::GreaterThanGreaterThanEqualsToken
            | Kind::GreaterThanGreaterThanGreaterThanEqualsToken
            | Kind::GreaterThanGreaterThanGreaterThanToken
            | Kind::GreaterThanGreaterThanToken
    )
}

// scanner.go:111
fn should_rescan_jsx_identifier(node: P<Node>) -> bool {
    if let Some(parent) = node.parent() {
        match parent.kind() {
            Kind::JsxAttribute
            | Kind::JsxOpeningElement
            | Kind::JsxClosingElement
            | Kind::JsxSelfClosingElement
            | Kind::JsxNamespacedName => {
                // May parse an identifier like `module-layout`; that will be scanned as a keyword at first, but we should parse the whole thing to get an identifier.
                return ast::is_keyword_kind(node.kind()) || node.kind() == Kind::Identifier;
            }
            Kind::PropertyAccessExpression => {
                // The leftmost name of a dotted JSX tag name (e.g. `a-b` in `<a-b.c>`) may contain hyphens, so rescan it as a JSX identifier.
                return (ast::is_keyword_kind(node.kind()) || node.kind() == Kind::Identifier)
                    && is_leftmost_jsx_tag_name(node);
            }
            _ => {}
        }
    }
    false
}

// scanner.go:129
fn is_leftmost_jsx_tag_name(node: P<Node>) -> bool {
    ast::find_ancestor_or_quit(node, |n| {
        if n.parent().is_none() {
            FindAncestorResult::Quit
        } else if ast::is_jsx_tag_name(n) {
            FindAncestorResult::True
        } else if ast::is_property_access_expression(n.parent().unwrap())
            && n.parent().unwrap().expression() == Some(n)
        {
            FindAncestorResult::False
        } else {
            FindAncestorResult::Quit
        }
    })
    .is_some()
}

impl FormattingScanner {
    // scanner.go:144
    fn should_rescan_jsx_text(&self, node: P<Node>) -> bool {
        if ast::is_jsx_text(node) {
            return true;
        }
        if !ast::is_jsx_element(node) || !self.has_last_token_info {
            return false;
        }

        self.last_token_info.token.kind == Kind::JsxText
    }
}

// scanner.go:155
fn should_rescan_slash_token(container: P<Node>) -> bool {
    container.kind() == Kind::RegularExpressionLiteral
}

// scanner.go:159
fn should_rescan_template_token(container: P<Node>) -> bool {
    container.kind() == Kind::TemplateMiddle || container.kind() == Kind::TemplateTail
}

// scanner.go:164
fn should_rescan_jsx_attribute_value(node: P<Node>) -> bool {
    node.parent().is_some_and(|p| ast::is_jsx_attribute(p) && p.initializer() == Some(node))
}

// scanner.go:168
fn starts_with_slash_token(t: Kind) -> bool {
    t == Kind::SlashToken || t == Kind::SlashEqualsToken
}

// scanner.go:172
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScanAction {
    Scan,
    RescanGreaterThanToken,
    RescanSlashToken,
    RescanTemplateToken,
    RescanJsxIdentifier,
    RescanJsxText,
    RescanJsxAttributeValue,
}

// scanner.go:184
fn fix_token_kind(mut token_info: TokenInfo, container: P<Node>) -> TokenInfo {
    if ast::is_token_kind(container.kind()) && token_info.token.kind != container.kind() {
        token_info.token.kind = container.kind();
    }
    token_info
}

impl FormattingScanner {
    // scanner.go:191
    pub(crate) fn read_token_info(&mut self, n: P<Node>) -> TokenInfo {
        assert!(self.is_on_token());

        // normally scanner returns the smallest available token
        // check the kind of context node to determine if scanner should have more greedy behavior and consume more text.

        let expected_scan_action = if should_rescan_greater_than_token(n) {
            ScanAction::RescanGreaterThanToken
        } else if should_rescan_slash_token(n) {
            ScanAction::RescanSlashToken
        } else if should_rescan_template_token(n) {
            ScanAction::RescanTemplateToken
        } else if should_rescan_jsx_identifier(n) {
            ScanAction::RescanJsxIdentifier
        } else if self.should_rescan_jsx_text(n) {
            ScanAction::RescanJsxText
        } else if should_rescan_jsx_attribute_value(n) {
            ScanAction::RescanJsxAttributeValue
        } else {
            ScanAction::Scan
        };

        if self.has_last_token_info && expected_scan_action == self.last_scan_action {
            // readTokenInfo was called before with the same expected scan action.
            // No need to re-scan text, return existing 'lastTokenInfo'
            // it is ok to call fixTokenKind here since it does not affect
            // what portion of text is consumed. In contrast rescanning can change it,
            // i.e. for '>=' when originally scanner eats just one character
            // and rescanning forces it to consume more.
            self.last_token_info = fix_token_kind(std::mem::take(&mut self.last_token_info), n);
            return self.last_token_info.clone();
        }

        if self.s.token_full_start() != self.saved_pos {
            // readTokenInfo was called before but scan action differs - rescan text
            self.s.reset_token_state(self.saved_pos);
            self.s.scan();
        }

        let mut current_token = self.get_next_token(n, expected_scan_action);

        let token =
            new_text_range_with_kind(self.s.token_full_start(), self.s.token_end(), current_token);

        // consume trailing trivia
        self.trailing_trivia = Vec::new();
        while self.s.token_full_start() < self.end_pos {
            current_token = self.s.scan();
            if !ast::is_trivia(current_token) {
                break;
            }
            let trivia = new_text_range_with_kind(
                self.s.token_full_start(),
                self.s.token_end(),
                current_token,
            );

            self.trailing_trivia.push(trivia);

            if current_token == Kind::NewLineTrivia {
                // move past new line
                self.s.scan();
                break;
            }
        }

        self.has_last_token_info = true;
        self.last_token_info = TokenInfo {
            leading_trivia: self.leading_trivia.clone(),
            token,
            trailing_trivia: self.trailing_trivia.clone(),
        };
        self.last_token_info = fix_token_kind(std::mem::take(&mut self.last_token_info), n);

        self.last_token_info.clone()
    }

    // scanner.go:272
    fn get_next_token(&mut self, n: P<Node>, expected_scan_action: ScanAction) -> Kind {
        let token = self.s.token();
        self.last_scan_action = ScanAction::Scan;
        match expected_scan_action {
            ScanAction::RescanGreaterThanToken => {
                if token == Kind::GreaterThanToken {
                    self.last_scan_action = ScanAction::RescanGreaterThanToken;
                    let new_token = self.s.re_scan_greater_than_token();
                    assert_eq!(n.kind(), new_token);
                    return new_token;
                }
            }
            ScanAction::RescanSlashToken => {
                if starts_with_slash_token(token) {
                    self.last_scan_action = ScanAction::RescanSlashToken;
                    let new_token = self.s.re_scan_slash_token(false);
                    assert_eq!(n.kind(), new_token);
                    return new_token;
                }
            }
            ScanAction::RescanTemplateToken => {
                if token == Kind::CloseBraceToken {
                    self.last_scan_action = ScanAction::RescanTemplateToken;
                    return self.s.re_scan_template_token(false /*isTaggedTemplate*/);
                }
            }
            ScanAction::RescanJsxIdentifier => {
                self.last_scan_action = ScanAction::RescanJsxIdentifier;
                return self.s.scan_jsx_identifier();
            }
            ScanAction::RescanJsxText => {
                self.last_scan_action = ScanAction::RescanJsxText;
                return self.s.re_scan_jsx_token(false /*allowMultilineJsxText*/);
            }
            ScanAction::RescanJsxAttributeValue => {
                self.last_scan_action = ScanAction::RescanJsxAttributeValue;
                return self.s.re_scan_jsx_attribute_value();
            }
            ScanAction::Scan => {
                // no rescan needed; the token was already produced by the normal scan
            }
        }
        token
    }

    // scanner.go:312
    pub(crate) fn read_eof_token_range(&self) -> TextRangeWithKind {
        assert!(self.is_on_eof());
        new_text_range_with_kind(self.s.token_full_start(), self.s.token_end(), Kind::EndOfFile)
    }

    // scanner.go:321
    pub(crate) fn is_on_token(&self) -> bool {
        let mut current = self.s.token();
        if self.has_last_token_info {
            current = self.last_token_info.token.kind;
        }
        current != Kind::EndOfFile && !ast::is_trivia(current)
    }

    // scanner.go:329
    pub(crate) fn is_on_eof(&self) -> bool {
        let mut current = self.s.token();
        if self.has_last_token_info {
            current = self.last_token_info.token.kind;
        }
        current == Kind::EndOfFile
    }

    // scanner.go:337
    pub(crate) fn skip_to_end_of(&mut self, r: TextRange) {
        self.s.reset_token_state(r.end().expect_source("formatting ranges have source positions"));
        self.saved_pos = self.s.token_full_start();
        self.last_scan_action = ScanAction::Scan;
        self.has_last_token_info = false;
        self.was_new_line = false;
        self.leading_trivia = Vec::new();
        self.trailing_trivia = Vec::new();
    }

    // scanner.go:347
    pub(crate) fn skip_to_start_of(&mut self, r: TextRange) {
        self.s.reset_token_state(r.pos().expect_source("formatting ranges have source positions"));
        self.saved_pos = self.s.token_full_start();
        self.last_scan_action = ScanAction::Scan;
        self.has_last_token_info = false;
        self.was_new_line = false;
        self.leading_trivia = Vec::new();
        self.trailing_trivia = Vec::new();
    }

    // scanner.go:357
    pub(crate) fn get_current_leading_trivia(&self) -> Vec<TextRangeWithKind> {
        self.leading_trivia.clone()
    }

    // scanner.go:361
    pub(crate) fn last_trailing_trivia_was_new_line(&self) -> bool {
        self.was_new_line
    }

    // scanner.go:365
    pub(crate) fn get_token_full_start(&self) -> u32 {
        if self.has_last_token_info {
            return self
                .last_token_info
                .token
                .loc
                .pos()
                .expect_source("formatting tokens have source positions");
        }
        self.s.token_full_start()
    }
}
