//! Port of tsgolint's `internal/rules/dot_notation/dot_notation.go`.

use std::borrow::Cow;

use regex::{Regex, RegexBuilder};

use tsrs_ast::{self as ast, Kind, ModifierFlags, Node};
use tsrs_core::P;
use tsrs_core::json::Value;

use crate::protocol::{Fix, Range, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};

const ES3_KEYWORDS: &[&str] = &[
    "abstract",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "double",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "function",
    "goto",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "volatile",
    "while",
    "with",
];

fn build_use_dot_message(key: &str) -> RuleMessage {
    RuleMessage {
        id: "useDot".into(),
        description: format!("[{key}] is better written in dot notation."),
        help: String::new(),
    }
}

fn build_use_brackets_message(key: &str) -> RuleMessage {
    RuleMessage {
        id: "useBrackets".into(),
        description: format!(".{key} is a syntax error."),
        help: String::new(),
    }
}

fn is_dot_notation_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_' || first == b'$')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$')
}

fn get_computed_property_value(node: P<Node>) -> Option<(&'static str, Cow<'static, str>)> {
    match node.kind() {
        Kind::StringLiteral => Some((node.text(), format!("\"{}\"", node.text()).into())),
        Kind::TrueKeyword => Some(("true", "true".into())),
        Kind::FalseKeyword => Some(("false", "false".into())),
        Kind::NullKeyword => Some(("null", "null".into())),
        Kind::NoSubstitutionTemplateLiteral => {
            Some((node.text(), format!("`{}`", node.text()).into()))
        }
        _ => None,
    }
}

fn get_static_property_name(node: P<Node>) -> Option<&'static str> {
    matches!(node.kind(), Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral)
        .then(|| node.text())
}

fn is_decimal_integer(node: P<Node>) -> bool {
    if node.kind() != Kind::NumericLiteral {
        return false;
    }
    let text = node.text();
    !text.is_empty()
        && text.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_digit() || byte == b'_' && index > 0 && index < text.len() - 1
        })
}

fn needs_space_after_identifier(text: &str, end: u32) -> bool {
    let next = tsrs_scanner::skip_trivia(text, end);
    next == end
        && text
            .get(next as usize..)
            .and_then(|tail| tail.chars().next())
            .is_some_and(|rune| tsrs_scanner::is_identifier_part(u32::from(rune)))
}

fn token_range(context: &RuleContext<'_>, pos: u32) -> Range {
    let range = tsrs_scanner::get_range_of_token_at_position(context.source_file, pos);
    Range { pos: range.source_pos().unwrap(), end: range.source_end().unwrap() }
}

fn go_repeat_length(tail: &str) -> Option<usize> {
    let bytes = tail.as_bytes();
    let mut index = 1;
    let integer = |index: &mut usize| {
        let start = *index;
        while bytes.get(*index).is_some_and(u8::is_ascii_digit) {
            *index += 1;
        }
        *index > start && (*index == start + 1 || bytes[start] != b'0')
    };
    if !integer(&mut index) {
        return None;
    }
    if bytes.get(index) == Some(&b',') {
        index += 1;
        if bytes.get(index) != Some(&b'}') && !integer(&mut index) {
            return None;
        }
    }
    (bytes.get(index) == Some(&b'}')).then_some(index + 1)
}

fn go_pattern(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut index = 0;
    let mut in_class = false;
    let mut first_in_class = false;
    let mut class_negation = false;
    let mut last_class_set = false;
    while index < pattern.len() {
        let tail = &pattern[index..];
        if !in_class && tail.starts_with("\\Q") {
            let literal = &tail[2..];
            if let Some(end) = literal.find("\\E") {
                out.push_str(&regex::escape(&literal[..end]));
                index += 2 + end + 2;
            } else {
                out.push_str(&regex::escape(literal));
                break;
            }
            continue;
        }
        if !in_class
            && (tail.starts_with("(?P<") || tail.starts_with("(?<"))
            && let Some(end) = tail.find('>')
        {
            out.push_str("(?:");
            index += end + 1;
            continue;
        }
        let ch = tail.chars().next().unwrap();
        if ch == '\\' {
            if tail.starts_with("\\p") || tail.starts_with("\\P") {
                let property_tail = &tail[2..];
                let property = if let Some(braced) = property_tail.strip_prefix('{') {
                    braced.find('}').map(|end| (&braced[..end], end + 4))
                } else {
                    property_tail
                        .chars()
                        .next()
                        .map(|rune| (&property_tail[..rune.len_utf8()], rune.len_utf8() + 2))
                };
                if let Some((property, length)) = property {
                    let inverted = tail.starts_with("\\P") ^ property.starts_with('^');
                    let property = property.strip_prefix('^').unwrap_or(property);
                    if property == "Cs" {
                        out.push_str(match (in_class, inverted) {
                            (true, true) => "\\s\\S",
                            (true, false) => "\\x{10ffff}",
                            (false, true) => "[\\s\\S]",
                            (false, false) => "[a&&b]",
                        });
                    } else {
                        out.push_str(if inverted { "\\P{" } else { "\\p{" });
                        out.push_str(property);
                        out.push('}');
                    }
                    index += length;
                    first_in_class = false;
                    last_class_set = in_class;
                    continue;
                }
            }
            if tail.starts_with("\\x{")
                && let Some(end) = tail.find('}')
            {
                out.push_str(&tail[..=end]);
                index += end + 1;
                first_in_class = false;
                last_class_set = false;
                continue;
            }
            out.push(ch);
            index += 1;
            if let Some(next) = pattern[index..].chars().next() {
                out.push(next);
                index += next.len_utf8();
                last_class_set = in_class && matches!(next, 'd' | 'D' | 'w' | 'W' | 's' | 'S');
            }
            first_in_class = false;
            continue;
        }
        if !in_class && ch == '{' {
            if let Some(length) = go_repeat_length(tail) {
                out.push_str(&tail[..length]);
                index += length;
            } else {
                out.push_str("\\{");
                index += 1;
            }
            continue;
        }
        if !in_class && ch == '}' {
            out.push_str("\\}");
            index += 1;
            continue;
        }
        if in_class
            && tail.starts_with("[:")
            && let Some(end) = tail.find(":]")
        {
            out.push_str(&tail[..end + 2]);
            index += end + 2;
            first_in_class = false;
            last_class_set = true;
            continue;
        }
        match ch {
            '[' if !in_class => {
                in_class = true;
                first_in_class = true;
                class_negation = false;
                out.push(ch);
            }
            '[' if in_class && !tail.starts_with("[:") => {
                out.push_str("\\[");
                first_in_class = false;
            }
            ']' if in_class && first_in_class => {
                out.push_str("\\]");
                first_in_class = false;
            }
            ']' if in_class => {
                in_class = false;
                out.push(ch);
            }
            '^' if in_class && first_in_class && !class_negation => {
                class_negation = true;
                out.push(ch);
            }
            '&' | '~' if in_class => {
                out.push('\\');
                out.push(ch);
                first_in_class = false;
            }
            '-' if in_class && (last_class_set || tail.starts_with("--")) => {
                out.push_str("\\-");
                first_in_class = false;
            }
            _ => {
                out.push(ch);
                if in_class {
                    first_in_class = false;
                }
            }
        }
        last_class_set = false;
        index += ch.len_utf8();
    }
    out
}

pub struct DotNotation {
    allow_keywords: bool,
    allow_private_class_property_access: bool,
    allow_protected_class_property_access: bool,
    allow_index_signature_property_access: bool,
    allow_pattern: Option<Regex>,
}

impl DotNotation {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("dot-notation: options must be an object".into()),
        };
        let boolean = |name: &str, default| match object.and_then(|object| object.get(name)) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(format!("dot-notation: {name} must be a boolean")),
        };
        let pattern = match object.and_then(|object| object.get("allowPattern")) {
            None | Some(Value::Null) => "",
            Some(Value::String(pattern)) => pattern.as_str(),
            _ => return Err("dot-notation: allowPattern must be a string".into()),
        };
        let allow_pattern = if pattern.is_empty() {
            None
        } else {
            Some(RegexBuilder::new(&go_pattern(pattern)).octal(true).build().map_err(|error| {
                format!("dot-notation: invalid allowPattern {pattern:?}: {error}")
            })?)
        };
        Ok(Self {
            allow_keywords: boolean("allowKeywords", true)?,
            allow_private_class_property_access: boolean("allowPrivateClassPropertyAccess", false)?,
            allow_protected_class_property_access: boolean(
                "allowProtectedClassPropertyAccess",
                false,
            )?,
            allow_index_signature_property_access: boolean(
                "allowIndexSignaturePropertyAccess",
                false,
            )?,
            allow_pattern,
        })
    }

    fn report_computed_property(&self, node: P<Node>, context: &mut RuleContext<'_>) {
        let element = node.as_element_access_expression();
        let property = element.argument_expression;
        let Some((value, formatted)) = get_computed_property_value(property) else {
            return;
        };
        if !is_dot_notation_identifier(value)
            || !self.allow_keywords && ES3_KEYWORDS.contains(&value)
            || self.allow_pattern.as_ref().is_some_and(|pattern| pattern.is_match(value))
        {
            return;
        }
        let fixes = if context.options.fix {
            let object_range = trim_node_range(context.source_file, element.expression);
            let property_range = trim_node_range(context.source_file, property);
            let left_bracket = token_range(context, object_range.end);
            let right_bracket = token_range(context, property_range.end);
            let mut start = left_bracket.pos;
            let mut fixes = Vec::with_capacity(3);
            if let Some(question_dot) = element.question_dot_token() {
                start = question_dot.end().expect_source("parsed optional access");
            } else {
                fixes.push(Fix {
                    text: if is_decimal_integer(element.expression) { " ." } else { "." }.into(),
                    range: Range { pos: left_bracket.pos, end: left_bracket.pos },
                });
            }
            fixes.push(Fix {
                text: value.into(),
                range: Range { pos: start, end: right_bracket.end },
            });
            let end = node.end().expect_source("parsed element access");
            if needs_space_after_identifier(context.source_file.text(), end) {
                fixes.push(Fix { text: " ".into(), range: Range { pos: end, end } });
            }
            fixes
        } else {
            Vec::new()
        };
        context.report_diagnostic(crate::protocol::Diagnostic {
            range: Some(trim_node_range(context.source_file, property)),
            message: build_use_dot_message(&formatted),
            fixes,
            ..Default::default()
        });
    }

    fn report_dot_keyword_access(&self, node: P<Node>, context: &mut RuleContext<'_>) {
        if self.allow_keywords {
            return;
        }
        let access = node.as_property_access_expression();
        let property = access.name;
        if property.kind() != Kind::Identifier || !ES3_KEYWORDS.contains(&property.text()) {
            return;
        }
        let mut fixes = Vec::new();
        if context.options.fix
            && !(access.question_dot_token().is_none()
                && access.expression.kind() == Kind::Identifier
                && access.expression.text() == "let")
        {
            let object_range = trim_node_range(context.source_file, access.expression);
            let dot_range = token_range(context, object_range.end);
            if access.question_dot_token().is_none() {
                fixes.push(Fix { text: String::new(), range: dot_range });
            }
            fixes.push(Fix {
                text: format!("[\"{}\"]", property.text()),
                range: trim_node_range(context.source_file, property),
            });
        }
        context.report_diagnostic(crate::protocol::Diagnostic {
            range: Some(trim_node_range(context.source_file, property)),
            message: build_use_brackets_message(property.text()),
            fixes,
            ..Default::default()
        });
    }

    fn should_skip_for_type_aware_allowances(
        &self,
        node: P<Node>,
        context: &mut RuleContext<'_>,
    ) -> bool {
        let index_access = self.allow_index_signature_property_access
            || context.program.options().no_property_access_from_index_signature.is_true();
        if !self.allow_private_class_property_access
            && !self.allow_protected_class_property_access
            && !index_access
        {
            return false;
        }
        let access = node.as_element_access_expression();
        let property = access.argument_expression;
        let mut symbol = context.checker.get_symbol_at_location_exported(property);
        if symbol.is_none()
            && let Some(name) = get_static_property_name(property)
        {
            let t = context.checker.get_type_at_location(access.expression);
            let object = context.checker.get_non_nullable_type(t);
            symbol = context
                .checker
                .get_properties_of_type(object)
                .iter()
                .find(|candidate| candidate.name() == name)
                .copied();
        }
        let declaration = symbol.and_then(|symbol| {
            let declarations = symbol.declarations();
            let getter = declarations
                .iter()
                .copied()
                .find(|declaration| declaration.kind() == Kind::GetAccessor);
            let setter = declarations
                .iter()
                .copied()
                .find(|declaration| declaration.kind() == Kind::SetAccessor);
            if getter.is_none() && setter.is_none() {
                declarations.first().copied()
            } else if ast::is_write_access(node) {
                setter.or(getter)
            } else {
                getter.or(setter)
            }
        });
        if let Some(declaration) = declaration {
            if self.allow_private_class_property_access
                && ast::has_modifier(declaration, ModifierFlags::Private)
            {
                return true;
            }
            if self.allow_protected_class_property_access
                && ast::has_modifier(declaration, ModifierFlags::Protected)
            {
                return true;
            }
            if index_access && declaration.kind() == Kind::IndexSignature {
                return true;
            }
        }
        if symbol.is_none_or(|symbol| symbol.declarations().is_empty()) && index_access {
            if context.program.options().no_property_access_from_index_signature.is_true() {
                return true;
            }
            let t = context.checker.get_type_at_location(access.expression);
            let object = context.checker.get_non_nullable_type(t);
            let object = context.checker.get_apparent_type(object);
            let key = context.checker.get_type_at_location(property);
            let base_key = context.checker.get_base_type_of_literal_type(key);
            if context.checker.get_index_type_of_type(object, key).is_some()
                || context.checker.get_index_type_of_type(object, base_key).is_some()
            {
                return true;
            }
        }
        false
    }
}

impl Rule for DotNotation {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Exit(Kind::ElementAccessExpression)
                | Listener::Enter(Kind::PropertyAccessExpression)
        )
    }
    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match listener {
            Listener::Exit(_) => {
                if !self.should_skip_for_type_aware_allowances(node, context) {
                    self.report_computed_property(node, context);
                }
            }
            Listener::Enter(_) => self.report_dot_keyword_access(node, context),
            _ => {}
        }
    }
}
