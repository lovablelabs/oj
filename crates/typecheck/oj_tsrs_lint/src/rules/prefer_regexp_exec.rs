use rustc_hash::FxHashSet;
use tsrs_ast::{self as ast, Kind, Node, NodeFlags, Symbol};
use tsrs_checker::{Checker, Type};
use tsrs_core::P;

use crate::protocol::{Fix, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, replace_node, trim_node_range};
use crate::utils::{get_type_name, is_strong_precedence_node, union_type_parts};

pub struct PreferRegexpExec;

fn build_reg_exp_exec_over_string_match_message() -> RuleMessage {
    RuleMessage {
        id: "regExpExecOverStringMatch".into(),
        description: "Use the `RegExp#exec()` method instead.".into(),
        ..Default::default()
    }
}

const ARGUMENT_TYPE_STRING: u8 = 2;
const ARGUMENT_TYPE_REG_EXP: u8 = 4;

#[derive(Default)]
enum StaticArgumentValue {
    #[default]
    Unknown,
    String,
    RegExp(&'static str),
    Other,
}

fn is_node_parenthesized(node: P<Node>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == Kind::ParenthesizedExpression
            && parent.as_parenthesized_expression().expression() == node
    })
}

fn is_weak_precedence_parent(node: P<Node>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        Kind::PostfixUnaryExpression
        | Kind::PrefixUnaryExpression
        | Kind::BinaryExpression
        | Kind::ConditionalExpression
        | Kind::AwaitExpression => true,
        Kind::PropertyAccessExpression
        | Kind::ElementAccessExpression
        | Kind::CallExpression
        | Kind::NewExpression => parent.expression() == Some(node),
        Kind::TaggedTemplateExpression => parent.as_tagged_template_expression().tag == node,
        _ => false,
    }
}

fn build_wrapping_fix(
    context: &RuleContext<'_>,
    node: P<Node>,
    inner_nodes: [P<Node>; 2],
    wrap: impl FnOnce(&str, &str) -> String,
) -> Fix {
    let codes = inner_nodes.map(|inner| {
        let range = trim_node_range(context.source_file, inner);
        let text = &context.source_file.text()[range.pos as usize..range.end as usize];
        if is_strong_precedence_node(inner) { text.to_owned() } else { format!("({text})") }
    });
    let mut code = wrap(&codes[0], &codes[1]);
    if is_weak_precedence_parent(node) && !is_node_parenthesized(node) {
        code = format!("({code})");
    }
    replace_node(context.source_file, node, code)
}

fn extract_regex_literal_flags(node: P<Node>) -> Option<&'static str> {
    if node.kind() != Kind::RegularExpressionLiteral {
        return None;
    }
    let text = node.as_regular_expression_literal().text();
    if text.len() < 2 || !text.starts_with('/') {
        return None;
    }
    let bytes = text.as_bytes();
    let closing_slash = (1..bytes.len()).rev().find(|&index| {
        bytes[index] == b'/'
            && bytes[..index].iter().rev().take_while(|&&byte| byte == b'\\').count() % 2 == 0
    })?;
    Some(&text[closing_slash + 1..])
}

fn is_string_literal(node: P<Node>) -> bool {
    node.kind() == Kind::StringLiteral
}

fn is_reg_exp_constructor_call(node: P<Node>) -> bool {
    matches!(node.kind(), Kind::CallExpression | Kind::NewExpression)
        && node
            .expression()
            .is_some_and(|callee| callee.kind() == Kind::Identifier && callee.text() == "RegExp")
}

fn definitely_does_not_contain_global_flag(node: P<Node>) -> bool {
    let node = ast::skip_parentheses(node);
    if !is_reg_exp_constructor_call(node) {
        return false;
    }
    let Some(&flags) = node.arguments().get(1) else {
        return true;
    };
    let flags = ast::skip_parentheses(flags);
    if flags.kind() == Kind::VoidExpression
        || flags.kind() == Kind::Identifier && flags.text() == "undefined"
    {
        return true;
    }
    is_string_literal(flags) && !flags.text().contains('g')
}

fn get_static_argument_value(
    checker: &mut Checker,
    node: P<Node>,
    visited: &mut FxHashSet<P<Symbol>>,
) -> StaticArgumentValue {
    let node = ast::skip_parentheses(node);
    match node.kind() {
        Kind::StringLiteral => return StaticArgumentValue::String,
        Kind::RegularExpressionLiteral => {
            return extract_regex_literal_flags(node)
                .map_or(StaticArgumentValue::Unknown, StaticArgumentValue::RegExp);
        }
        Kind::NoSubstitutionTemplateLiteral
        | Kind::NumericLiteral
        | Kind::TrueKeyword
        | Kind::FalseKeyword
        | Kind::NullKeyword => return StaticArgumentValue::Other,
        Kind::Identifier => {
            let Some(symbol) = checker.get_symbol_at_location_exported(node) else {
                return StaticArgumentValue::Unknown;
            };
            if !visited.insert(symbol) {
                return StaticArgumentValue::Unknown;
            }
            let result = symbol.value_declaration().and_then(|declaration| {
                if declaration.kind() != Kind::VariableDeclaration
                    || declaration.parent().is_none_or(|parent| {
                        parent.kind() != Kind::VariableDeclarationList
                            || !parent.flags().intersects(NodeFlags::Const)
                    })
                {
                    return None;
                }
                declaration
                    .initializer()
                    .map(|initializer| get_static_argument_value(checker, initializer, visited))
            });
            visited.remove(&symbol);
            return result.unwrap_or_default();
        }
        Kind::AsExpression | Kind::TypeAssertionExpression | Kind::NonNullExpression => {
            return get_static_argument_value(checker, node.expression().unwrap(), visited);
        }
        _ => {}
    }
    if is_reg_exp_constructor_call(node) {
        let arguments = node.arguments();
        if arguments.first().is_some_and(|&arg| !is_string_literal(arg))
            || arguments.get(1).is_some_and(|&arg| !is_string_literal(arg))
            || arguments.first().is_some_and(|arg| !is_valid_regexp_pattern(arg.text()))
        {
            return StaticArgumentValue::Unknown;
        }
        return StaticArgumentValue::RegExp(arguments.get(1).map_or("", |arg| arg.text()));
    }
    StaticArgumentValue::Unknown
}

fn collect_argument_types(checker: &mut Checker, types: &[P<Type>]) -> u8 {
    let mut result = 0;
    for &ty in types {
        match get_type_name(checker, ty).as_ref() {
            "RegExp" => result |= ARGUMENT_TYPE_REG_EXP,
            "string" => result |= ARGUMENT_TYPE_STRING,
            _ => {}
        }
    }
    result
}

fn report_with_fix(
    context: &mut RuleContext<'_>,
    report_node: P<Node>,
    call_node: P<Node>,
    object_node: P<Node>,
    argument_node: P<Node>,
    build_expression: impl FnOnce(&str, &str) -> String,
) {
    let fixes = if context.options.fix {
        vec![build_wrapping_fix(context, call_node, [object_node, argument_node], build_expression)]
    } else {
        Vec::new()
    };
    context.report_diagnostic(crate::protocol::Diagnostic {
        range: Some(trim_node_range(context.source_file, report_node)),
        message: build_reg_exp_exec_over_string_match_message(),
        fixes,
        ..Default::default()
    });
}

impl Rule for PreferRegexpExec {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::CallExpression)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        if node.arguments().len() != 1 {
            return;
        }
        let callee = node.as_call_expression().expression;
        if !matches!(callee.kind(), Kind::PropertyAccessExpression | Kind::ElementAccessExpression)
        {
            return;
        }
        let (property_name, ok) = context.checker.get_accessed_property_name(callee);
        if !ok || property_name != "match" {
            return;
        }
        let object_node = callee.expression().unwrap();
        let object_type = context.checker.get_type_at_location(object_node);
        if get_type_name(context.checker, object_type) != "string" {
            return;
        }
        let argument_node = node.arguments()[0];
        let static_argument =
            get_static_argument_value(context.checker, argument_node, &mut FxHashSet::default());
        let argument_type = context.checker.get_type_at_location(argument_node);
        let argument_types =
            collect_argument_types(context.checker, union_type_parts(&argument_type));
        if matches!(static_argument, StaticArgumentValue::RegExp(flags) if flags.contains('g'))
            || matches!(static_argument, StaticArgumentValue::Unknown)
                && argument_types & ARGUMENT_TYPE_REG_EXP != 0
                && !definitely_does_not_contain_global_flag(argument_node)
        {
            return;
        }
        let report_node = if callee.kind() == Kind::PropertyAccessExpression {
            callee.as_property_access_expression().name()
        } else {
            callee.as_element_access_expression().argument_expression
        };
        if is_string_literal(argument_node) {
            let pattern = argument_node.text();
            if !is_valid_regexp_pattern(pattern) {
                return;
            }
            let literal = format!("/{}/", pattern.replace('/', "\\/"));
            report_with_fix(context, report_node, node, object_node, argument_node, |object, _| {
                format!("{literal}.exec({object})")
            });
            return;
        }
        match argument_types {
            ARGUMENT_TYPE_REG_EXP => {
                report_with_fix(
                    context,
                    report_node,
                    node,
                    object_node,
                    argument_node,
                    |object, arg| format!("{arg}.exec({object})"),
                );
            }
            ARGUMENT_TYPE_STRING => {
                report_with_fix(
                    context,
                    report_node,
                    node,
                    object_node,
                    argument_node,
                    |object, arg| format!("RegExp({arg}).exec({object})"),
                );
            }
            _ => {}
        }
    }
}

const OPTION_N: u8 = 1;
const OPTION_X: u8 = 2;
const OPTION_U: u8 = 4;

struct PatternGroup {
    options: u8,
    conditional: bool,
    alternatives: usize,
    condition_pending: bool,
    expression_conditional: bool,
    condition_name: Option<String>,
}

enum PatternReference {
    Name(String),
    Number(usize),
    Malformed,
}

// regexp2's ECMAScript mode also accepts .NET group syntax and class subtraction.
struct RegexpPattern {
    chars: Vec<char>,
    pos: usize,
    options: u8,
    captures: usize,
    names: FxHashSet<String>,
    references: Vec<(PatternReference, bool)>,
    numeric_conditions: Vec<usize>,
    option_conditions: Vec<String>,
    groups: Vec<PatternGroup>,
    ignore_next_paren: bool,
}

fn is_valid_regexp_pattern(pattern: &str) -> bool {
    RegexpPattern {
        chars: pattern.chars().collect(),
        pos: 0,
        options: 0,
        captures: 0,
        names: FxHashSet::default(),
        references: Vec::new(),
        numeric_conditions: Vec::new(),
        option_conditions: Vec::new(),
        groups: Vec::new(),
        ignore_next_paren: false,
    }
    .scan_regex()
    .is_some()
}

impl RegexpPattern {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += 1;
        Some(ch)
    }

    fn eat(&mut self, ch: char) -> bool {
        if self.peek() != Some(ch) {
            return false;
        }
        self.pos += 1;
        true
    }

    fn scan_blank(&mut self) -> Option<()> {
        loop {
            if self.options & OPTION_X != 0 {
                while self
                    .peek()
                    .is_some_and(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}'))
                {
                    self.pos += 1;
                }
                if self.eat('#') {
                    while self.peek().is_some_and(|ch| ch != '\n') {
                        self.pos += 1;
                    }
                    continue;
                }
            }
            if self.chars.get(self.pos..self.pos + 3) == Some(&['(', '?', '#'][..]) {
                self.pos += 3;
                while self.peek().is_some_and(|ch| ch != ')') {
                    self.pos += 1;
                }
                if !self.eat(')') {
                    return None;
                }
            } else {
                return Some(());
            }
        }
    }

    fn scan_decimal(&mut self) -> Option<usize> {
        let mut value = 0usize;
        while let Some(ch @ '0'..='9') = self.peek() {
            self.pos += 1;
            value = value.checked_mul(10)?.checked_add(ch as usize - '0' as usize)?;
            if value > i32::MAX as usize {
                return None;
            }
        }
        Some(value)
    }

    fn is_true_quantifier(&self) -> bool {
        if matches!(self.peek(), Some('*' | '+' | '?')) {
            return true;
        }
        if self.peek() != Some('{') {
            return false;
        }
        let mut pos = self.pos + 1;
        let start = pos;
        while self.chars.get(pos).is_some_and(char::is_ascii_digit) {
            pos += 1;
        }
        if pos == start {
            return false;
        }
        if self.chars.get(pos) == Some(&',') {
            pos += 1;
            while self.chars.get(pos).is_some_and(char::is_ascii_digit) {
                pos += 1;
            }
        }
        self.chars.get(pos) == Some(&'}')
    }

    fn scan_regex(&mut self) -> Option<()> {
        while self.pos < self.chars.len() {
            self.scan_blank()?;
            if self.pos == self.chars.len() {
                break;
            }
            if self.is_true_quantifier() {
                return None;
            }
            let ch = self.next()?;
            match ch {
                '[' => self.scan_char_set()?,
                '(' => {
                    self.scan_group_open()?;
                    continue;
                }
                ')' => {
                    let group = self.groups.pop()?;
                    if group.conditional && group.alternatives > 1 {
                        return None;
                    }
                    self.options = group.options;
                    if let Some(parent) = self.groups.last_mut()
                        && std::mem::take(&mut parent.condition_pending)
                    {
                        continue;
                    }
                }
                '|' => {
                    if let Some(group) = self.groups.last_mut() {
                        group.alternatives += 1;
                    }
                    continue;
                }
                '\\' => self.scan_backslash()?,
                _ => {}
            }
            self.scan_blank()?;
            if !self.is_true_quantifier() {
                continue;
            }
            match self.next()? {
                '{' => {
                    let min = self.scan_decimal()?;
                    let max = if self.eat(',') {
                        if self.peek() == Some('}') {
                            i32::MAX as usize
                        } else {
                            self.scan_decimal()?
                        }
                    } else {
                        min
                    };
                    if !self.eat('}') || min > max {
                        return None;
                    }
                }
                '*' | '+' | '?' => {}
                _ => return None,
            }
            self.scan_blank()?;
            self.eat('?');
        }
        if !self.groups.is_empty()
            || self.option_conditions.iter().any(|name| !self.names.contains(name))
            || self.references.iter().any(|(reference, unicode)| {
                (!self.names.is_empty() || *unicode)
                    && match reference {
                        PatternReference::Name(name) => !self.names.contains(name),
                        PatternReference::Number(number) => *number > self.captures,
                        PatternReference::Malformed => true,
                    }
            })
            || self.numeric_conditions.iter().any(|&number| number > self.captures)
        {
            return None;
        }
        Some(())
    }

    fn scan_capname(&mut self) -> Option<String> {
        let mut name = String::new();
        while let Some(mut ch) = self.peek() {
            let escaped = ch == '\\';
            if escaped {
                self.pos += 1;
                if !self.eat('u') {
                    return None;
                }
                let value = if self.options & OPTION_U != 0 && self.eat('{') {
                    self.scan_hex_until_brace()?
                } else {
                    self.scan_hex(4)?
                };
                ch = char::from_u32(value)?;
            }
            let valid = if name.is_empty() {
                super::regexp_unicode::is_identifier_start(ch)
            } else {
                super::regexp_unicode::is_identifier_part(ch)
            };
            if !valid {
                if escaped {
                    return None;
                }
                break;
            }
            if !escaped {
                self.pos += 1;
            }
            name.push(ch);
        }
        Some(name)
    }

    fn scan_group_open(&mut self) -> Option<()> {
        let saved_options = self.options;
        let mut conditional = false;
        let mut condition_pending = false;
        let mut expression_conditional = false;
        let mut condition_name = None;
        let ignored = std::mem::take(&mut self.ignore_next_paren);
        if self.peek() != Some('?') || self.chars.get(self.pos + 1) == Some(&')') {
            if !ignored && self.options & OPTION_N == 0 {
                self.captures += 1;
            }
        } else {
            self.pos += 1;
            match self.next()? {
                ':' | '=' | '!' | '>' => {}
                opener @ ('<' | '\'') => {
                    if opener == '<' && matches!(self.peek(), Some('=' | '!')) {
                        self.pos += 1;
                    } else {
                        if ignored {
                            return None;
                        }
                        let name = self.scan_capname()?;
                        let close = if opener == '<' { '>' } else { '\'' };
                        if name.is_empty() || !self.eat(close) || !self.names.insert(name) {
                            return None;
                        }
                        self.captures += 1;
                    }
                }
                '(' => {
                    conditional = true;
                    if self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                        let number = self.scan_decimal()?;
                        if !self.eat(')') {
                            return None;
                        }
                        self.numeric_conditions.push(number);
                    } else {
                        let condition_start = self.pos;
                        if self.peek().is_some_and(super::regexp_unicode::is_word_char) {
                            let name = self.scan_capname()?;
                            if !name.is_empty() && self.eat(')') {
                                condition_name = Some(name);
                            }
                        }
                        if condition_name.is_none() {
                            self.pos = condition_start - 1;
                            self.ignore_next_paren = true;
                            condition_pending = true;
                            expression_conditional = true;
                            if self.chars.get(self.pos..self.pos + 3) == Some(&['(', '?', '#'][..])
                            {
                                return None;
                            }
                        }
                    }
                }
                _ => {
                    self.pos -= 1;
                    if ignored
                        || self.groups.last().is_some_and(|group| group.expression_conditional)
                    {
                        return None;
                    }
                    if let Some(name) =
                        self.groups.last().and_then(|group| group.condition_name.as_ref())
                    {
                        self.option_conditions.push(name.clone());
                    }
                    let mut off = false;
                    while let Some(ch) = self.peek() {
                        let option = match ch.to_ascii_lowercase() {
                            'n' => OPTION_N,
                            'x' => OPTION_X,
                            'u' => OPTION_U,
                            'i' | 'm' | 's' => 0,
                            '-' => {
                                off = true;
                                self.pos += 1;
                                continue;
                            }
                            '+' => {
                                off = false;
                                self.pos += 1;
                                continue;
                            }
                            _ => break,
                        };
                        if ignored {
                            return None;
                        }
                        self.pos += 1;
                        if off {
                            self.options &= !option;
                        } else {
                            self.options |= option;
                        }
                    }
                    if self.eat(')') {
                        return Some(());
                    }
                    if !self.eat(':') {
                        return None;
                    }
                }
            }
        }
        self.groups.push(PatternGroup {
            options: saved_options,
            conditional,
            alternatives: 0,
            condition_pending,
            expression_conditional,
            condition_name,
        });
        Some(())
    }

    fn scan_hex(&mut self, count: usize) -> Option<u32> {
        let mut value = 0;
        for _ in 0..count {
            value = value * 16 + self.next()?.to_digit(16)?;
        }
        Some(value)
    }

    fn scan_hex_until_brace(&mut self) -> Option<u32> {
        let mut value = 0u32;
        let mut any = false;
        while let Some(ch) = self.next() {
            if ch == '}' {
                return any.then_some(value);
            }
            any = true;
            value = value.checked_mul(16)?.checked_add(ch.to_digit(16)?)?;
            if value > 0x0010_ffff {
                return None;
            }
        }
        None
    }

    fn scan_char_escape(&mut self) -> Option<u32> {
        let ch = self.next()?;
        let start = self.pos;
        if matches!(ch, '0'..='7') {
            let mut value = ch as u32 - '0' as u32;
            for _ in 0..2 {
                if value >= 0x20 {
                    break;
                }
                let Some(digit @ '0'..='7') = self.peek() else {
                    break;
                };
                self.pos += 1;
                value = value * 8 + digit as u32 - '0' as u32;
            }
            return Some(value);
        }
        let result = match ch {
            'x' if self.peek() == Some('{') => return Some(ch as u32),
            'x' => self.scan_hex(2),
            'u' if self.options & OPTION_U != 0 && self.eat('{') => {
                return self.scan_hex_until_brace();
            }
            'u' => self.scan_hex(4),
            'a' => Some(7),
            'b' => Some(8),
            'e' => Some(27),
            'f' => Some(12),
            'n' => Some(10),
            'r' => Some(13),
            't' => Some(9),
            'v' => Some(11),
            'c' => self.next().and_then(|control| {
                (control.to_ascii_uppercase() as u32)
                    .checked_sub('@' as u32)
                    .filter(|&code| code < 32)
            }),
            _ => Some(ch as u32),
        };
        if result.is_none() {
            self.pos = start;
            return Some(ch as u32);
        }
        result
    }

    fn parse_property(&mut self) -> Option<()> {
        let property: String = if self.eat('{') {
            let mut property = String::new();
            while self.peek().is_some_and(|ch| ch.is_alphanumeric() || ch == '_' || ch == '-') {
                property.push(self.next()?);
            }
            if !self.eat('}') {
                return None;
            }
            property
        } else {
            if self.options & OPTION_U != 0 {
                return None;
            }
            self.next()?.to_string()
        };
        REGEXP_UNICODE_PROPERTIES.binary_search(&property.as_str()).ok().map(|_| ())
    }

    fn scan_backslash(&mut self) -> Option<()> {
        let ch = self.peek()?;
        match ch {
            'b' | 'B' | 'A' | 'G' | 'Z' | 'z' | 'w' | 'W' | 'd' | 'D' | 's' | 'S' => {
                self.pos += 1;
            }
            'p' | 'P' if self.options & OPTION_U != 0 => {
                self.pos += 1;
                self.parse_property()?;
            }
            'k' => {
                let start = self.pos;
                self.pos += 1;
                let reference = if !self.eat('<') {
                    Some(PatternReference::Malformed)
                } else if self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                    match self.scan_decimal() {
                        Some(number) if self.eat('>') => Some(PatternReference::Number(number)),
                        Some(_) => None,
                        None => Some(PatternReference::Malformed),
                    }
                } else {
                    Some(
                        self.scan_capname()
                            .filter(|name| !name.is_empty())
                            .filter(|_| self.eat('>'))
                            .map_or(PatternReference::Malformed, PatternReference::Name),
                    )
                };
                if let Some(reference) = reference {
                    self.references.push((reference, self.options & OPTION_U != 0));
                }
                self.pos = start;
                self.scan_char_escape()?;
            }
            '1'..='9' => {
                self.scan_decimal()?;
            }
            _ => {
                self.scan_char_escape()?;
            }
        }
        Some(())
    }

    fn scan_char_set(&mut self) -> Option<()> {
        self.eat('^');
        let mut range_start = None;
        let mut first = true;
        while let Some(ch) = self.next() {
            if ch == ']' {
                return Some(());
            }
            let mut translated = false;
            let mut value = ch as u32;
            if ch == '\\' && self.peek().is_some() {
                let escape = self.next()?;
                match escape {
                    'd' | 'D' | 's' | 'S' | 'w' | 'W' => {
                        range_start = None;
                        first = false;
                        continue;
                    }
                    'P' if range_start.is_some() && self.options & OPTION_U == 0 => return None,
                    'p' if self.options & OPTION_U == 0 => {
                        if let Some(start) = range_start.take() {
                            if start > 'p' as u32 {
                                return None;
                            }
                        } else if self.peek() == Some('-')
                            && self.chars.get(self.pos + 1).is_some_and(|&ch| ch != ']')
                        {
                            self.pos += 1;
                            if 'p' > self.next()? {
                                return None;
                            }
                        }
                        first = false;
                        continue;
                    }
                    'p' | 'P' => {
                        if range_start.is_some() {
                            return None;
                        }
                        self.parse_property()?;
                        first = false;
                        continue;
                    }
                    '-' => {
                        first = false;
                        continue;
                    }
                    _ => {
                        self.pos -= 1;
                        value = self.scan_char_escape()?;
                        translated = true;
                    }
                }
            } else if ch == '[' && range_start.is_none() && self.peek() == Some(':') {
                let saved = self.pos;
                self.pos += 1;
                self.eat('^');
                while self.peek().is_some_and(super::regexp_unicode::is_word_char) {
                    self.pos += 1;
                }
                if !self.eat(':') || !self.eat(']') {
                    self.pos = saved;
                }
            }
            if let Some(start) = range_start.take() {
                if ch == '[' && !translated && !first {
                    self.scan_char_set()?;
                    if self.peek().is_some_and(|ch| ch != ']') {
                        return None;
                    }
                } else if start > value {
                    return None;
                }
            } else if self.peek() == Some('-')
                && self.chars.get(self.pos + 1).is_some_and(|&ch| ch != ']')
            {
                range_start = Some(value);
                self.pos += 1;
            } else if ch == '-' && !translated && !first && self.eat('[') {
                self.scan_char_set()?;
                if self.peek().is_some_and(|ch| ch != ']') {
                    return None;
                }
            }
            first = false;
        }
        None
    }
}

// The pinned oracle uses Go 1.27's case-sensitive Unicode 17 property keys.
const REGEXP_UNICODE_PROPERTIES: &[&str] = &[
    "ASCII_Hex_Digit",
    "Adlam",
    "Ahom",
    "Anatolian_Hieroglyphs",
    "Arabic",
    "Armenian",
    "Avestan",
    "Balinese",
    "Bamum",
    "Bassa_Vah",
    "Batak",
    "Bengali",
    "Beria_Erfe",
    "Bhaiksuki",
    "Bidi_Control",
    "Bopomofo",
    "Brahmi",
    "Braille",
    "Buginese",
    "Buhid",
    "C",
    "Canadian_Aboriginal",
    "Carian",
    "Cased_Letter",
    "Caucasian_Albanian",
    "Cc",
    "Cf",
    "Chakma",
    "Cham",
    "Cherokee",
    "Chorasmian",
    "Close_Punctuation",
    "Cn",
    "Co",
    "Combining_Mark",
    "Common",
    "Connector_Punctuation",
    "Control",
    "Coptic",
    "Cs",
    "Cuneiform",
    "Currency_Symbol",
    "Cypriot",
    "Cypro_Minoan",
    "Cyrillic",
    "Dash",
    "Dash_Punctuation",
    "Decimal_Number",
    "Deprecated",
    "Deseret",
    "Devanagari",
    "Diacritic",
    "Dives_Akuru",
    "Dogra",
    "Duployan",
    "Egyptian_Hieroglyphs",
    "Elbasan",
    "Elymaic",
    "Enclosing_Mark",
    "Ethiopic",
    "Extender",
    "Final_Punctuation",
    "Format",
    "Garay",
    "Georgian",
    "Glagolitic",
    "Gothic",
    "Grantha",
    "Greek",
    "Gujarati",
    "Gunjala_Gondi",
    "Gurmukhi",
    "Gurung_Khema",
    "Han",
    "Hangul",
    "Hanifi_Rohingya",
    "Hanunoo",
    "Hatran",
    "Hebrew",
    "Hex_Digit",
    "Hiragana",
    "Hyphen",
    "IDS_Binary_Operator",
    "IDS_Trinary_Operator",
    "IDS_Unary_Operator",
    "ID_Compat_Math_Continue",
    "ID_Compat_Math_Start",
    "Ideographic",
    "Imperial_Aramaic",
    "Inherited",
    "Initial_Punctuation",
    "Inscriptional_Pahlavi",
    "Inscriptional_Parthian",
    "Javanese",
    "Join_Control",
    "Kaithi",
    "Kannada",
    "Katakana",
    "Kawi",
    "Kayah_Li",
    "Kharoshthi",
    "Khitan_Small_Script",
    "Khmer",
    "Khojki",
    "Khudawadi",
    "Kirat_Rai",
    "L",
    "LC",
    "Lao",
    "Latin",
    "Lepcha",
    "Letter",
    "Letter_Number",
    "Limbu",
    "Line_Separator",
    "Linear_A",
    "Linear_B",
    "Lisu",
    "Ll",
    "Lm",
    "Lo",
    "Logical_Order_Exception",
    "Lowercase_Letter",
    "Lt",
    "Lu",
    "Lycian",
    "Lydian",
    "M",
    "Mahajani",
    "Makasar",
    "Malayalam",
    "Mandaic",
    "Manichaean",
    "Marchen",
    "Mark",
    "Masaram_Gondi",
    "Math_Symbol",
    "Mc",
    "Me",
    "Medefaidrin",
    "Meetei_Mayek",
    "Mende_Kikakui",
    "Meroitic_Cursive",
    "Meroitic_Hieroglyphs",
    "Miao",
    "Mn",
    "Modi",
    "Modifier_Combining_Mark",
    "Modifier_Letter",
    "Modifier_Symbol",
    "Mongolian",
    "Mro",
    "Multani",
    "Myanmar",
    "N",
    "Nabataean",
    "Nag_Mundari",
    "Nandinagari",
    "Nd",
    "New_Tai_Lue",
    "Newa",
    "Nko",
    "Nl",
    "No",
    "Noncharacter_Code_Point",
    "Nonspacing_Mark",
    "Number",
    "Nushu",
    "Nyiakeng_Puachue_Hmong",
    "Ogham",
    "Ol_Chiki",
    "Ol_Onal",
    "Old_Hungarian",
    "Old_Italic",
    "Old_North_Arabian",
    "Old_Permic",
    "Old_Persian",
    "Old_Sogdian",
    "Old_South_Arabian",
    "Old_Turkic",
    "Old_Uyghur",
    "Open_Punctuation",
    "Oriya",
    "Osage",
    "Osmanya",
    "Other",
    "Other_Alphabetic",
    "Other_Default_Ignorable_Code_Point",
    "Other_Grapheme_Extend",
    "Other_ID_Continue",
    "Other_ID_Start",
    "Other_Letter",
    "Other_Lowercase",
    "Other_Math",
    "Other_Number",
    "Other_Punctuation",
    "Other_Symbol",
    "Other_Uppercase",
    "P",
    "Pahawh_Hmong",
    "Palmyrene",
    "Paragraph_Separator",
    "Pattern_Syntax",
    "Pattern_White_Space",
    "Pau_Cin_Hau",
    "Pc",
    "Pd",
    "Pe",
    "Pf",
    "Phags_Pa",
    "Phoenician",
    "Pi",
    "Po",
    "Prepended_Concatenation_Mark",
    "Private_Use",
    "Ps",
    "Psalter_Pahlavi",
    "Punctuation",
    "Quotation_Mark",
    "Radical",
    "Regional_Indicator",
    "Rejang",
    "Runic",
    "S",
    "STerm",
    "Samaritan",
    "Saurashtra",
    "Sc",
    "Sentence_Terminal",
    "Separator",
    "Sharada",
    "Shavian",
    "Siddham",
    "Sidetic",
    "SignWriting",
    "Sinhala",
    "Sk",
    "Sm",
    "So",
    "Soft_Dotted",
    "Sogdian",
    "Sora_Sompeng",
    "Soyombo",
    "Space_Separator",
    "Spacing_Mark",
    "Sundanese",
    "Sunuwar",
    "Surrogate",
    "Syloti_Nagri",
    "Symbol",
    "Syriac",
    "Tagalog",
    "Tagbanwa",
    "Tai_Le",
    "Tai_Tham",
    "Tai_Viet",
    "Tai_Yo",
    "Takri",
    "Tamil",
    "Tangsa",
    "Tangut",
    "Telugu",
    "Terminal_Punctuation",
    "Thaana",
    "Thai",
    "Tibetan",
    "Tifinagh",
    "Tirhuta",
    "Titlecase_Letter",
    "Todhri",
    "Tolong_Siki",
    "Toto",
    "Tulu_Tigalari",
    "Ugaritic",
    "Unassigned",
    "Unified_Ideograph",
    "Uppercase_Letter",
    "Vai",
    "Variation_Selector",
    "Vithkuqi",
    "Wancho",
    "Warang_Citi",
    "White_Space",
    "Yezidi",
    "Yi",
    "Z",
    "Zanabazar_Square",
    "Zl",
    "Zp",
    "Zs",
    "cntrl",
    "digit",
    "punct",
];
