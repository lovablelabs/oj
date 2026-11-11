use tsrs_ast::{self as ast, Kind, Node, Symbol};
use tsrs_checker::Checker;
use tsrs_core::P;

use crate::protocol::{Fix, Range, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils::{get_constrained_type_at_location, quote_single_string_literal};

pub struct PreferIncludes;

fn build_prefer_includes_message() -> RuleMessage {
    RuleMessage {
        id: "preferIncludes".into(),
        description: "Use 'includes()' method instead.".into(),
        ..Default::default()
    }
}

fn build_prefer_string_includes_message() -> RuleMessage {
    RuleMessage {
        id: "preferStringIncludes".into(),
        description: "Use `String#includes()` method with a string instead.".into(),
        ..Default::default()
    }
}

fn pattern_string_literal(pattern: &str) -> String {
    let mut decoded = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            decoded.push(ch);
            continue;
        }
        let ch = chars.next().expect("simple pattern has no trailing backslash");
        match ch {
            'n' => decoded.push('\n'),
            'r' => decoded.push('\r'),
            't' => decoded.push('\t'),
            'v' => decoded.push('\u{b}'),
            'f' => decoded.push('\u{c}'),
            '0'..='7' => {
                let mut value = u32::from(ch) - u32::from('0');
                let max_digits = if ch <= '3' { 3 } else { 2 };
                for _ in 1..max_digits {
                    let Some(&next @ '0'..='7') = chars.peek() else {
                        break;
                    };
                    chars.next();
                    value = value * 8 + u32::from(next) - u32::from('0');
                }
                decoded.push(char::from_u32(value).unwrap());
            }
            _ => decoded.push(ch),
        }
    }
    quote_single_string_literal(&decoded)
}

fn is_simple_literal_pattern(pattern: &str) -> bool {
    let mut escaped = false;
    for ch in pattern.chars() {
        if escaped {
            escaped = false;
            if matches!(ch, 'd' | 'D' | 'w' | 'W' | 's' | 'S' | 'b' | 'B' | 'c' | 'x' | 'u') {
                return false;
            }
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if matches!(ch, '.' | '*' | '+' | '?' | '|' | '^' | '$' | '[' | ']' | '(' | ')' | '{' | '}')
        {
            return false;
        }
    }
    !escaped
}

fn is_valid_simple_literal_pattern(pattern: &str) -> bool {
    let pattern = String::from_utf8_lossy(pattern.as_bytes());
    if !is_simple_literal_pattern(&pattern) {
        return false;
    }
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            continue;
        }
        let Some(escaped) = chars.next() else {
            return false;
        };
        if !matches!(escaped, '1'..='9') {
            continue;
        }
        let mut number = escaped as i32 - '0' as i32;
        for digit in chars.clone().take_while(char::is_ascii_digit) {
            let Some(next) =
                number.checked_mul(10).and_then(|n| n.checked_add(digit as i32 - '0' as i32))
            else {
                return false;
            };
            number = next;
        }
    }
    true
}

fn extract_regex_literal_pattern(node: P<Node>) -> &'static str {
    if node.kind() != Kind::RegularExpressionLiteral {
        return "";
    }
    let text = node.as_regular_expression_literal().text();
    if text.len() < 3 || !text.starts_with('/') {
        return "";
    }
    let Some(last_slash) = text.rfind('/').filter(|&index| index > 0) else {
        return "";
    };
    let pattern = &text[1..last_slash];
    if last_slash + 1 != text.len() || !is_valid_simple_literal_pattern(pattern) {
        return "";
    }
    pattern
}

fn extract_reg_exp_constructor_pattern(node: P<Node>) -> &'static str {
    if node.kind() != Kind::NewExpression {
        return "";
    }
    let expression = node.as_new_expression().expression;
    if expression.kind() != Kind::Identifier || expression.text() != "RegExp" {
        return "";
    }
    let Some(&argument) = node.arguments().first() else {
        return "";
    };
    if argument.kind() != Kind::StringLiteral {
        return "";
    }
    let pattern = argument.as_string_literal().text();
    if !is_valid_simple_literal_pattern(pattern) {
        return "";
    }
    pattern
}

fn resolve_variable_pattern(checker: &mut Checker, node: P<Node>) -> &'static str {
    if !ast::is_identifier(node) {
        return "";
    }
    let Some(declaration) =
        checker.get_symbol_at_location_exported(node).and_then(|symbol| symbol.value_declaration())
    else {
        return "";
    };
    if declaration.kind() != Kind::VariableDeclaration {
        return "";
    }
    let Some(initializer) = declaration.initializer() else {
        return "";
    };
    let pattern = extract_regex_literal_pattern(initializer);
    if !pattern.is_empty() {
        return pattern;
    }
    extract_reg_exp_constructor_pattern(initializer)
}

fn resolve_regex_pattern(checker: &mut Checker, node: P<Node>) -> &'static str {
    let pattern = extract_regex_literal_pattern(node);
    if !pattern.is_empty() {
        return pattern;
    }
    resolve_variable_pattern(checker, node)
}

fn has_same_parameters(decl_a: P<Node>, decl_b: P<Node>) -> bool {
    if !ast::is_function_like(decl_a) || !ast::is_function_like(decl_b) {
        return false;
    }
    let params_a = decl_a.parameters();
    let params_b = decl_b.parameters();
    if params_a.len() != params_b.len() {
        return false;
    }
    params_a.iter().zip(params_b).all(|(&param_a, &param_b)| {
        let Some(source_file_a) = ast::get_source_file_of_node(param_a) else {
            return false;
        };
        let Some(source_file_b) = ast::get_source_file_of_node(param_b) else {
            return false;
        };
        let range_a = param_a.loc();
        let range_b = param_b.loc();
        source_file_a.text()
            [range_a.source_pos().unwrap() as usize..range_a.source_end().unwrap() as usize]
            == source_file_b.text()
                [range_b.source_pos().unwrap() as usize..range_b.source_end().unwrap() as usize]
    })
}

fn index_of_has_compatible_includes(checker: &mut Checker, symbol: P<Symbol>) -> bool {
    if symbol.declarations().is_empty() {
        return false;
    }
    symbol.declarations().iter().all(|&index_of_decl| {
        let Some(type_decl) = index_of_decl.parent() else {
            return false;
        };
        let ty = checker.get_type_at_location(type_decl);
        let Some(includes_symbol) = checker.get_property_of_type(ty, "includes") else {
            return false;
        };
        includes_symbol
            .declarations()
            .iter()
            .any(|&includes_decl| has_same_parameters(index_of_decl, includes_decl))
    })
}

fn is_number_literal(node: P<Node>, value: i64) -> bool {
    if node.kind() == Kind::NumericLiteral {
        return node.as_numeric_literal().text().parse::<i64>() == Ok(value);
    }
    if node.kind() == Kind::PrefixUnaryExpression {
        let prefix = node.as_prefix_unary_expression();
        if prefix.operator == Kind::MinusToken && prefix.operand.kind() == Kind::NumericLiteral {
            return prefix
                .operand
                .as_numeric_literal()
                .text()
                .parse::<i64>()
                .ok()
                .and_then(i64::checked_neg)
                == Some(value);
        }
    }
    false
}

fn is_positive_check(node: P<Node>) -> bool {
    let binary = node.as_binary_expression();
    match binary.operator_token.kind() {
        Kind::ExclamationEqualsEqualsToken
        | Kind::ExclamationEqualsToken
        | Kind::GreaterThanToken => is_number_literal(binary.right(), -1),
        Kind::GreaterThanEqualsToken => is_number_literal(binary.right(), 0),
        _ => false,
    }
}

fn is_negative_check(node: P<Node>) -> bool {
    let binary = node.as_binary_expression();
    match binary.operator_token.kind() {
        Kind::EqualsEqualsEqualsToken | Kind::EqualsEqualsToken | Kind::LessThanEqualsToken => {
            is_number_literal(binary.right(), -1)
        }
        Kind::LessThanToken => is_number_literal(binary.right(), 0),
        _ => false,
    }
}

impl PreferIncludes {
    fn check_call(node: P<Node>, context: &mut RuleContext<'_>) {
        let expression = node.as_call_expression().expression;
        if expression.kind() != Kind::PropertyAccessExpression {
            return;
        }
        let property = expression.as_property_access_expression();
        let name = property.name();
        if !ast::is_identifier(name) || name.text() != "test" || node.arguments().len() != 1 {
            return;
        }
        let pattern = resolve_regex_pattern(context.checker, property.expression);
        if pattern.is_empty() {
            return;
        }
        let argument = node.arguments()[0];
        let arg_type = get_constrained_type_at_location(context.checker, argument);
        if context
            .checker
            .get_property_of_type(arg_type, "includes")
            .is_none_or(|symbol| symbol.declarations().is_empty())
        {
            return;
        }
        if std::str::from_utf8(pattern.as_bytes()).is_err() {
            context.report_node(node, build_prefer_string_includes_message());
            return;
        }
        let file = context.source_file;
        context.report_node_with_fixes(node, build_prefer_string_includes_message(), || {
            let needs_parens = !matches!(
                argument.kind(),
                Kind::Identifier
                    | Kind::StringLiteral
                    | Kind::NumericLiteral
                    | Kind::NoSubstitutionTemplateLiteral
                    | Kind::PropertyAccessExpression
                    | Kind::CallExpression
                    | Kind::ElementAccessExpression
                    | Kind::ParenthesizedExpression
            );
            let call_range = trim_node_range(file, node);
            let arg_start = argument.pos().expect_source("parsed lint argument");
            let arg_end = argument.end().expect_source("parsed lint argument");
            let mut fixes = vec![
                Fix { range: Range { pos: call_range.pos, end: arg_start }, text: String::new() },
                Fix { range: Range { pos: arg_end, end: call_range.end }, text: String::new() },
            ];
            if needs_parens {
                fixes.push(Fix {
                    range: Range { pos: arg_start, end: arg_start },
                    text: "(".into(),
                });
                fixes.push(Fix { range: Range { pos: arg_end, end: arg_end }, text: ")".into() });
            }
            fixes.push(Fix {
                range: Range { pos: arg_end, end: arg_end },
                text: format!(".includes({})", pattern_string_literal(pattern)),
            });
            fixes
        });
    }

    fn check_binary(node: P<Node>, context: &mut RuleContext<'_>) {
        let binary = node.as_binary_expression();
        let left = binary.left;
        if left.kind() != Kind::CallExpression {
            return;
        }
        let expression = left.as_call_expression().expression;
        if expression.kind() != Kind::PropertyAccessExpression {
            return;
        }
        let name = expression.as_property_access_expression().name();
        if !ast::is_identifier(name) || name.text() != "indexOf" {
            return;
        }
        let is_positive = is_positive_check(node);
        let is_negative = is_negative_check(node);
        if !is_positive && !is_negative {
            return;
        }
        let Some(symbol) = context.checker.get_symbol_at_location_exported(name) else {
            return;
        };
        if !index_of_has_compatible_includes(context.checker, symbol) {
            return;
        }
        let file = context.source_file;
        context.report_node_with_fixes(node, build_prefer_includes_message(), || {
            let mut fixes = vec![
                Fix { range: trim_node_range(file, name), text: "includes".into() },
                Fix {
                    range: Range {
                        pos: left.end().expect_source("parsed lint call"),
                        end: node.end().expect_source("parsed lint comparison"),
                    },
                    text: String::new(),
                },
            ];
            if is_negative {
                let start = trim_node_range(file, left).pos;
                fixes.push(Fix { range: Range { pos: start, end: start }, text: "!".into() });
            }
            fixes
        });
    }
}

impl Rule for PreferIncludes {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(listener, Listener::Enter(Kind::CallExpression | Kind::BinaryExpression))
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match node.kind() {
            Kind::CallExpression => Self::check_call(node, context),
            Kind::BinaryExpression => Self::check_binary(node, context),
            _ => {}
        }
    }
}
