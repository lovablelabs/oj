use tsrs_ast::{self as ast, Kind, Node, SourceFile};
use tsrs_checker::Checker;
use tsrs_core::{P, json::Value};

use crate::go_quote;
use crate::protocol::{Fix, Range, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils;

pub struct PreferStringStartsEndsWith {
    allow_single_element_equality: bool,
}

struct ParsedRegExp {
    is_starts_with: bool,
    text: String,
}

#[derive(PartialEq, Eq)]
enum NullishKind {
    Unknown,
    Null,
    Undefined,
}

fn message(starts: bool) -> RuleMessage {
    RuleMessage {
        id: if starts { "preferStartsWith" } else { "preferEndsWith" }.into(),
        description: if starts {
            "Use 'String#startsWith' method instead."
        } else {
            "Use 'String#endsWith' method instead."
        }
        .into(),
        ..Default::default()
    }
}

fn is_negative_equality_operator(kind: Kind) -> bool {
    matches!(kind, Kind::ExclamationEqualsToken | Kind::ExclamationEqualsEqualsToken)
}

fn is_loose_equality_operator(kind: Kind) -> bool {
    matches!(kind, Kind::EqualsEqualsToken | Kind::ExclamationEqualsToken)
}

fn member_object(node: P<Node>) -> Option<P<Node>> {
    match node.kind() {
        Kind::PropertyAccessExpression => Some(node.as_property_access_expression().expression),
        Kind::ElementAccessExpression => Some(node.as_element_access_expression().expression),
        _ => None,
    }
}

fn member_optional_operator(node: P<Node>) -> &'static str {
    let optional = match node.kind() {
        Kind::PropertyAccessExpression => {
            node.as_property_access_expression().question_dot_token().is_some()
        }
        Kind::ElementAccessExpression => {
            node.as_element_access_expression().question_dot_token().is_some()
        }
        _ => false,
    };
    if optional { "?." } else { "." }
}

fn get_property_name(node: P<Node>) -> Option<&'static str> {
    match node.kind() {
        Kind::PropertyAccessExpression => Some(node.as_property_access_expression().name().text()),
        Kind::ElementAccessExpression => {
            let arg =
                ast::skip_parentheses(node.as_element_access_expression().argument_expression);
            matches!(arg.kind(), Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral)
                .then(|| arg.text())
        }
        _ => None,
    }
}

fn call_member_callee(node: P<Node>) -> Option<P<Node>> {
    let expression = ast::skip_parentheses(node.as_call_expression().expression);
    member_object(expression).map(|_| expression)
}

fn static_number(node: P<Node>) -> Option<f64> {
    let node = ast::skip_parentheses(node);
    if node.kind() == Kind::NumericLiteral {
        let number = node.as_numeric_literal().text().replace('_', "").parse::<f64>().ok()?;
        return number.is_finite().then_some(number);
    }
    if node.kind() == Kind::PrefixUnaryExpression {
        let prefix = node.as_prefix_unary_expression();
        if prefix.operator == Kind::MinusToken {
            return static_number(prefix.operand).map(|number| -number);
        }
    }
    None
}

fn variable_initializer(checker: &mut Checker, node: P<Node>) -> Option<P<Node>> {
    let declaration = checker.get_symbol_at_location_exported(node)?.value_declaration()?;
    if declaration.kind() != Kind::VariableDeclaration {
        return None;
    }
    declaration.initializer()
}

fn static_string(checker: &mut Checker, node: P<Node>) -> Option<&'static str> {
    let node = ast::skip_parentheses(node);
    match node.kind() {
        Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral => Some(node.text()),
        Kind::Identifier => {
            let initializer = variable_initializer(checker, node)?;
            static_string(checker, initializer)
        }
        _ => None,
    }
}

fn get_nullish_kind(checker: &mut Checker, node: P<Node>) -> NullishKind {
    let node = ast::skip_parentheses(node);
    if node.kind() == Kind::NullKeyword {
        return NullishKind::Null;
    }
    if node.kind() == Kind::VoidExpression
        || node.kind() == Kind::Identifier && node.text() == "undefined"
    {
        return NullishKind::Undefined;
    }
    if node.kind() == Kind::Identifier
        && let Some(initializer) = variable_initializer(checker, node)
    {
        return get_nullish_kind(checker, initializer);
    }
    NullishKind::Unknown
}

fn split_regex_literal(text: &str) -> Option<(&str, &str)> {
    if text.len() < 2 || !text.starts_with('/') {
        return None;
    }
    let bytes = text.as_bytes();
    for i in (1..bytes.len()).rev() {
        if bytes[i] == b'/' {
            let backslashes = bytes[..i].iter().rev().take_while(|&&ch| ch == b'\\').count();
            if backslashes % 2 == 0 {
                return Some((&text[1..i], &text[i + 1..]));
            }
        }
    }
    None
}

fn parse_reg_exp_text(pattern: &str) -> Option<String> {
    let starts = pattern.starts_with('^');
    let ends = pattern.ends_with('$');
    if starts == ends {
        return None;
    }
    let content = if starts { &pattern[1..] } else { &pattern[..pattern.len() - 1] };
    let mut text = String::new();
    let mut escaped = false;
    for ch in content.chars() {
        if escaped {
            let ch = match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'v' => '\u{b}',
                'f' => '\u{c}',
                '0' => '\0',
                'd' | 'D' | 'w' | 'W' | 's' | 'S' | 'b' | 'B' | 'c' | 'x' | 'u' | 'k' | 'p'
                | 'P' => return None,
                ch if unicode_is_digit(ch) => return None,
                ch => ch,
            };
            text.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if matches!(ch, '.' | '*' | '+' | '?' | '|' | '^' | '$' | '[' | ']' | '(' | ')' | '{' | '}')
        {
            return None;
        }
        text.push(ch);
    }
    (!escaped).then_some(text)
}

fn parse_reg_exp(checker: &mut Checker, node: P<Node>) -> Option<ParsedRegExp> {
    let node = ast::skip_parentheses(node);
    let (pattern, flags) = match node.kind() {
        Kind::RegularExpressionLiteral => {
            split_regex_literal(node.as_regular_expression_literal().text())?
        }
        Kind::NewExpression => {
            let expression = node.as_new_expression().expression;
            if expression.kind() != Kind::Identifier || expression.text() != "RegExp" {
                return None;
            }
            let args = node.arguments();
            if args.is_empty() || args[0].kind() != Kind::StringLiteral {
                return None;
            }
            let flags = if args.len() > 1 {
                if args[1].kind() != Kind::StringLiteral {
                    return None;
                }
                args[1].text()
            } else {
                ""
            };
            (args[0].text(), flags)
        }
        Kind::Identifier => {
            let initializer = variable_initializer(checker, node)?;
            return parse_reg_exp(checker, initializer);
        }
        _ => return None,
    };
    if flags.contains(['i', 'm', 'g', 'y']) {
        return None;
    }
    Some(ParsedRegExp {
        is_starts_with: pattern.starts_with('^'),
        text: parse_reg_exp_text(pattern)?,
    })
}

fn fix(pos: u32, end: u32, text: impl Into<String>) -> Fix {
    Fix { range: Range { pos, end }, text: text.into() }
}

struct Helpers<'a> {
    file: P<SourceFile>,
    checker: &'a mut Checker,
}

impl Helpers<'_> {
    fn normalized_node_text(&self, node: P<Node>) -> String {
        let range = trim_node_range(self.file, ast::skip_parentheses(node));
        self.file.text()[range.pos as usize..range.end as usize]
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect()
    }

    fn is_same_tokens(&self, a: P<Node>, b: P<Node>) -> bool {
        self.normalized_node_text(a) == self.normalized_node_text(b)
    }

    fn get_property_range(&self, node: P<Node>) -> Range {
        let range = trim_node_range(self.file, node);
        let Some(obj) = member_object(node) else {
            return range;
        };
        let start = tsrs_scanner::get_range_of_token_at_position(
            self.file,
            obj.end().expect_source("parsed lint member"),
        );
        Range { pos: start.source_pos().unwrap(), end: range.end }
    }

    fn is_string_type(&mut self, node: P<Node>) -> bool {
        let t = self.checker.get_type_at_location(ast::skip_parentheses(node));
        utils::get_type_name(self.checker, t) == "string"
    }

    #[expect(
        clippy::float_cmp,
        reason = "The pinned rule compares the numeric operand exactly with the UTF-16 length"
    )]
    fn is_length_expression(&mut self, node: P<Node>, expected: P<Node>) -> bool {
        let node = ast::skip_parentheses(node);
        let expected = ast::skip_parentheses(expected);
        if get_property_name(node) == Some("length")
            && member_object(node).is_some_and(|obj| self.is_same_tokens(obj, expected))
        {
            return true;
        }
        match (static_number(node), static_string(self.checker, expected)) {
            (Some(length), Some(string)) => length == string.encode_utf16().count() as f64,
            _ => false,
        }
    }

    fn is_length_ahead_of_end(
        &mut self,
        node: P<Node>,
        substring: P<Node>,
        parent: P<Node>,
    ) -> bool {
        let node = ast::skip_parentheses(node);
        match node.kind() {
            Kind::PrefixUnaryExpression => {
                let prefix = node.as_prefix_unary_expression();
                prefix.operator == Kind::MinusToken
                    && self.is_length_expression(prefix.operand, substring)
            }
            Kind::BinaryExpression => {
                let bin = node.as_binary_expression();
                bin.operator_token.kind() == Kind::MinusToken
                    && self.is_length_expression(bin.left, parent)
                    && self.is_length_expression(bin.right(), substring)
            }
            _ => false,
        }
    }

    fn is_last_index_expression(&mut self, node: P<Node>, expected: P<Node>) -> bool {
        let node = ast::skip_parentheses(node);
        if node.kind() != Kind::BinaryExpression {
            return false;
        }
        let bin = node.as_binary_expression();
        bin.operator_token.kind() == Kind::MinusToken
            && self.is_length_expression(bin.left, expected)
            && static_number(bin.right()) == Some(1.0)
    }

    fn fix_with_right_operand(&self, node: P<Node>, member: P<Node>, starts: bool) -> Vec<Fix> {
        let bin = node.as_binary_expression();
        let property = self.get_property_range(member);
        let right = trim_node_range(self.file, ast::skip_parentheses(bin.right()));
        let range = trim_node_range(self.file, node);
        let mut fixes = Vec::new();
        if is_negative_equality_operator(bin.operator_token.kind()) {
            fixes.push(fix(range.pos, range.pos, "!"));
        }
        let kind = if starts { "start" } else { "end" };
        fixes.push(fix(
            property.pos,
            right.pos,
            format!("{}{kind}sWith(", member_optional_operator(member)),
        ));
        fixes.push(fix(right.end, range.end, ")"));
        fixes
    }

    fn fix_with_argument(
        &self,
        node: P<Node>,
        call: P<Node>,
        member: P<Node>,
        starts: bool,
    ) -> Vec<Fix> {
        let property = self.get_property_range(member);
        let call_range = trim_node_range(self.file, call);
        let range = trim_node_range(self.file, node);
        let mut fixes = Vec::new();
        if is_negative_equality_operator(node.as_binary_expression().operator_token.kind()) {
            fixes.push(fix(range.pos, range.pos, "!"));
        }
        let kind = if starts { "start" } else { "end" };
        fixes.push(fix(
            property.pos,
            property.end,
            format!("{}{kind}sWith", member_optional_operator(member)),
        ));
        fixes.push(fix(call_range.end, range.end, ""));
        fixes
    }

    fn report_string_index_or_char_at(
        &mut self,
        node: P<Node>,
        member: P<Node>,
        index: P<Node>,
        allow: bool,
        fixing: bool,
    ) -> Option<(bool, Vec<Fix>)> {
        let obj = member_object(member)?;
        if !self.is_string_type(obj) {
            return None;
        }
        let ends = self.is_last_index_expression(index, obj);
        if allow && ends {
            return None;
        }
        let starts = !ends && static_number(index) == Some(0.0);
        if allow && starts || !starts && !ends {
            return None;
        }
        let fixes = if fixing
            && static_string(self.checker, node.as_binary_expression().right())
                .is_some_and(|value| value.encode_utf16().count() == 1)
        {
            self.fix_with_right_operand(node, member, starts)
        } else {
            Vec::new()
        };
        Some((starts, fixes))
    }

    fn handle_binary_call_expression(
        &mut self,
        node: P<Node>,
        call: P<Node>,
        allow: bool,
        fixing: bool,
    ) -> Option<(bool, Vec<Fix>)> {
        let member = call_member_callee(call)?;
        let name = get_property_name(member)?;
        let args = call.arguments();
        let bin = node.as_binary_expression();
        let obj = member_object(member)?;
        match name {
            "charAt" => {
                if args.len() != 1 {
                    return None;
                }
                self.report_string_index_or_char_at(node, member, args[0], allow, fixing)
            }
            "indexOf" => {
                if args.len() != 1
                    || static_number(bin.right()) != Some(0.0)
                    || !self.is_string_type(obj)
                {
                    return None;
                }
                Some((
                    true,
                    if fixing {
                        self.fix_with_argument(node, call, member, true)
                    } else {
                        Vec::new()
                    },
                ))
            }
            "lastIndexOf" => {
                if args.len() != 1 || bin.right().kind() != Kind::BinaryExpression {
                    return None;
                }
                let right = bin.right().as_binary_expression();
                if right.operator_token.kind() != Kind::MinusToken
                    || !self.is_string_type(obj)
                    || !self.is_length_expression(right.left, obj)
                    || !self.is_length_expression(right.right(), args[0])
                {
                    return None;
                }
                Some((
                    false,
                    if fixing {
                        self.fix_with_argument(node, call, member, false)
                    } else {
                        Vec::new()
                    },
                ))
            }
            "match" => {
                if args.len() != 1 {
                    return None;
                }
                let nullish = get_nullish_kind(self.checker, bin.right());
                if !self.is_string_type(obj)
                    || nullish == NullishKind::Unknown
                    || !is_loose_equality_operator(bin.operator_token.kind())
                        && nullish != NullishKind::Null
                {
                    return None;
                }
                let parsed = parse_reg_exp(self.checker, args[0])?;
                let mut fixes = Vec::new();
                if fixing {
                    let range = trim_node_range(self.file, node);
                    if !is_negative_equality_operator(bin.operator_token.kind()) {
                        fixes.push(fix(range.pos, range.pos, "!"));
                    }
                    let property = self.get_property_range(member);
                    let arg = trim_node_range(self.file, args[0]);
                    let method = if parsed.is_starts_with { "startsWith" } else { "endsWith" };
                    fixes.push(fix(
                        property.pos,
                        property.end,
                        format!("{}{method}", member_optional_operator(member)),
                    ));
                    fixes.push(fix(arg.pos, arg.end, go_quote::quote(&parsed.text)));
                    fixes.push(fix(trim_node_range(self.file, call).end, range.end, ""));
                }
                Some((parsed.is_starts_with, fixes))
            }
            "slice" | "substring" => {
                if !self.is_string_type(obj) {
                    return None;
                }
                let mut starts = false;
                let mut ends = false;
                if args.len() == 1 {
                    ends = self.is_length_ahead_of_end(args[0], bin.right(), obj);
                } else if args.len() == 2 {
                    if static_number(args[0]) == Some(0.0)
                        && self.is_length_expression(args[1], bin.right())
                    {
                        starts = true;
                    } else if (self.is_length_expression(args[1], obj)
                        || static_number(args[1]) == Some(0.0))
                        && self.is_length_ahead_of_end(args[0], bin.right(), obj)
                    {
                        ends = true;
                    }
                }
                if !starts && !ends {
                    return None;
                }
                let mut can_fix = fixing;
                if is_loose_equality_operator(bin.operator_token.kind())
                    && ast::skip_parentheses(bin.right()).kind() != Kind::StringLiteral
                {
                    can_fix = false;
                }
                if can_fix {
                    if starts {
                        can_fix =
                            args.len() >= 2 && self.is_length_expression(args[1], bin.right());
                    } else {
                        let pos = ast::skip_parentheses(args[0]);
                        can_fix = if pos.kind() == Kind::BinaryExpression {
                            let pos = pos.as_binary_expression();
                            pos.operator_token.kind() == Kind::MinusToken
                                && self.is_length_expression(pos.left, obj)
                                && self.is_length_expression(pos.right(), bin.right())
                        } else if name == "slice" && pos.kind() == Kind::PrefixUnaryExpression {
                            let prefix = pos.as_prefix_unary_expression();
                            prefix.operator == Kind::MinusToken
                                && self.is_length_expression(prefix.operand, bin.right())
                        } else {
                            false
                        };
                    }
                }
                Some((
                    starts,
                    if can_fix {
                        self.fix_with_right_operand(node, member, starts)
                    } else {
                        Vec::new()
                    },
                ))
            }
            _ => None,
        }
    }

    fn handle_binary(
        &mut self,
        node: P<Node>,
        allow: bool,
        fixing: bool,
    ) -> Option<(bool, Vec<Fix>)> {
        let bin = node.as_binary_expression();
        if !matches!(
            bin.operator_token.kind(),
            Kind::EqualsEqualsToken
                | Kind::EqualsEqualsEqualsToken
                | Kind::ExclamationEqualsToken
                | Kind::ExclamationEqualsEqualsToken
        ) {
            return None;
        }
        let left = ast::skip_parentheses(bin.left);
        match left.kind() {
            Kind::ElementAccessExpression => self.report_string_index_or_char_at(
                node,
                left,
                left.as_element_access_expression().argument_expression,
                allow,
                fixing,
            ),
            Kind::CallExpression => self.handle_binary_call_expression(node, left, allow, fixing),
            _ => None,
        }
    }

    fn handle_regex_test_call(&mut self, node: P<Node>, fixing: bool) -> Option<(bool, Vec<Fix>)> {
        let args = node.arguments();
        if args.len() != 1 {
            return None;
        }
        let member = call_member_callee(node)?;
        if get_property_name(member) != Some("test") {
            return None;
        }
        let parsed = parse_reg_exp(self.checker, member_object(member)?)?;
        let mut fixes = Vec::new();
        if fixing {
            let arg = ast::skip_parentheses(args[0]);
            let needs_paren = !ast::is_literal_expression(arg)
                && !matches!(
                    arg.kind(),
                    Kind::NoSubstitutionTemplateLiteral
                        | Kind::TemplateExpression
                        | Kind::Identifier
                        | Kind::PropertyAccessExpression
                        | Kind::ElementAccessExpression
                        | Kind::CallExpression
                );
            let call_range = trim_node_range(self.file, node);
            let arg_range = trim_node_range(self.file, arg);
            fixes.push(fix(call_range.pos, arg_range.pos, ""));
            if needs_paren {
                fixes.push(fix(arg_range.pos, arg_range.pos, "("));
                fixes.push(fix(arg_range.end, arg_range.end, ")"));
            }
            let method = if parsed.is_starts_with { "startsWith" } else { "endsWith" };
            fixes.push(fix(
                arg_range.end,
                arg_range.end,
                format!(
                    "{}{method}({}",
                    member_optional_operator(member),
                    go_quote::quote(&parsed.text)
                ),
            ));
        }
        Some((parsed.is_starts_with, fixes))
    }
}

impl PreferStringStartsEndsWith {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("prefer-string-starts-ends-with: options must be an object".into()),
        };
        let allow_single_element_equality = match object.and_then(|object| object.get("allowSingleElementEquality")) {
            None | Some(Value::Null) => false,
            Some(Value::String(value)) if value == "always" => true,
            Some(Value::String(value)) if value == "never" => false,
            _ => return Err("prefer-string-starts-ends-with: allowSingleElementEquality must be 'always' or 'never'".into()),
        };
        Ok(Self { allow_single_element_equality })
    }
}

impl Rule for PreferStringStartsEndsWith {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(listener, Listener::Enter(Kind::BinaryExpression | Kind::CallExpression))
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let mut helpers = Helpers { file: context.source_file, checker: context.checker };
        let result = match node.kind() {
            Kind::BinaryExpression => {
                helpers.handle_binary(node, self.allow_single_element_equality, context.options.fix)
            }
            Kind::CallExpression => helpers.handle_regex_test_call(node, context.options.fix),
            _ => None,
        };
        if let Some((starts, fixes)) = result {
            context.report_node_with_fixes(node, message(starts), || fixes);
        }
    }
}

fn unicode_is_digit(ch: char) -> bool {
    matches!(ch as u32,
        0x0030..=0x0039 |
        0x0660..=0x0669 |
        0x06f0..=0x06f9 |
        0x07c0..=0x07c9 |
        0x0966..=0x096f |
        0x09e6..=0x09ef |
        0x0a66..=0x0a6f |
        0x0ae6..=0x0aef |
        0x0b66..=0x0b6f |
        0x0be6..=0x0bef |
        0x0c66..=0x0c6f |
        0x0ce6..=0x0cef |
        0x0d66..=0x0d6f |
        0x0de6..=0x0def |
        0x0e50..=0x0e59 |
        0x0ed0..=0x0ed9 |
        0x0f20..=0x0f29 |
        0x1040..=0x1049 |
        0x1090..=0x1099 |
        0x17e0..=0x17e9 |
        0x1810..=0x1819 |
        0x1946..=0x194f |
        0x19d0..=0x19d9 |
        0x1a80..=0x1a89 |
        0x1a90..=0x1a99 |
        0x1b50..=0x1b59 |
        0x1bb0..=0x1bb9 |
        0x1c40..=0x1c49 |
        0x1c50..=0x1c59 |
        0xa620..=0xa629 |
        0xa8d0..=0xa8d9 |
        0xa900..=0xa909 |
        0xa9d0..=0xa9d9 |
        0xa9f0..=0xa9f9 |
        0xaa50..=0xaa59 |
        0xabf0..=0xabf9 |
        0xff10..=0xff19 |
        0x104a0..=0x104a9 |
        0x10d30..=0x10d39 |
        0x10d40..=0x10d49 |
        0x11066..=0x1106f |
        0x110f0..=0x110f9 |
        0x11136..=0x1113f |
        0x111d0..=0x111d9 |
        0x112f0..=0x112f9 |
        0x11450..=0x11459 |
        0x114d0..=0x114d9 |
        0x11650..=0x11659 |
        0x116c0..=0x116c9 |
        0x116d0..=0x116e3 |
        0x11730..=0x11739 |
        0x118e0..=0x118e9 |
        0x11950..=0x11959 |
        0x11bf0..=0x11bf9 |
        0x11c50..=0x11c59 |
        0x11d50..=0x11d59 |
        0x11da0..=0x11da9 |
        0x11de0..=0x11de9 |
        0x11f50..=0x11f59 |
        0x16130..=0x16139 |
        0x16a60..=0x16a69 |
        0x16ac0..=0x16ac9 |
        0x16b50..=0x16b59 |
        0x16d70..=0x16d79 |
        0x1ccf0..=0x1ccf9 |
        0x1d7ce..=0x1d7ff |
        0x1e140..=0x1e149 |
        0x1e2f0..=0x1e2f9 |
        0x1e4f0..=0x1e4f9 |
        0x1e5f1..=0x1e5fa |
        0x1e950..=0x1e959 |
        0x1fbf0..=0x1fbf9
    )
}
