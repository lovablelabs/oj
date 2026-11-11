//! Port of tsgolint's `internal/rules/no_unnecessary_template_expression/no_unnecessary_template_expression.go`.

use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::{Type, TypeFlags};
use tsrs_core::{P, jsnum};

use crate::protocol::{Diagnostic, Fix, Range, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, replace_node, trim_node_range};
use crate::utils::{self, union_type_parts};

pub struct NoUnnecessaryTemplateExpression;

fn message() -> RuleMessage {
    RuleMessage {
        id: "noUnnecessaryTemplateExpression".into(),
        description: "Template literal expression is unnecessary and can be simplified.".into(),
        ..Default::default()
    }
}

fn is_underlying_type_string(t: P<Type>) -> bool {
    union_type_parts(&t).iter().all(|&part| {
        let parts = if part.flags().intersects(TypeFlags::Intersection) {
            part.types()
        } else {
            std::slice::from_ref(&part)
        };
        parts.iter().any(|part| part.flags().intersects(TypeFlags::StringLike))
    })
}

fn is_any_literal(node: P<Node>) -> bool {
    ast::is_literal_expression(node)
        || ast::is_boolean_literal(node)
        || node.kind() == Kind::NullKeyword
}

fn is_fixable_identifier(node: P<Node>) -> bool {
    if node.kind() == Kind::Identifier {
        matches!(node.text(), "undefined" | "Infinity" | "NaN")
    } else {
        node.kind() == Kind::UndefinedKeyword
    }
}

fn ends_with_unescaped_dollar_sign(text: &str) -> bool {
    text.ends_with('$')
        && text.as_bytes()[..text.len() - 1].iter().rev().take_while(|&&byte| byte == b'\\').count()
            % 2
            == 0
}

fn escape_template_raw_text(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'`' || byte == b'$' && bytes.get(index + 1) == Some(&b'{') {
            let backslashes =
                bytes[..index].iter().rev().take_while(|&&byte| byte == b'\\').count();
            if backslashes % 2 == 0 {
                result.push(b'\\');
            }
        }
        result.push(byte);
    }
    String::from_utf8(result).expect("inserting ASCII escapes preserves UTF-8")
}

fn node_text<'a>(context: &'a RuleContext<'_>, node: P<Node>) -> &'a str {
    let range = trim_node_range(context.source_file, node);
    &context.source_file.text()[range.pos as usize..range.end as usize]
}

fn raw_text(node: P<Node>) -> &'static str {
    node.template_literal_like_data().expect("template literal segment").raw_text
}

fn interpolation(span: P<Node>) -> (P<Node>, P<Node>) {
    if span.kind() == Kind::TemplateSpan {
        let span = span.as_template_span();
        (span.expression, span.literal)
    } else {
        let span = span.as_template_literal_type_span();
        (span.type_(), span.literal)
    }
}

fn unwrap_literal_type(node: P<Node>) -> P<Node> {
    if node.kind() == Kind::LiteralType { node.as_literal_type_node().literal } else { node }
}

fn remove(range: Range) -> Fix {
    Fix { range, text: String::new() }
}

fn report_with_fixes(context: &mut RuleContext<'_>, range: Range, fixes: Option<Vec<Fix>>) {
    let fixes = if context.options.fix {
        let Some(fixes) = fixes else {
            // Pinned headless output drops diagnostics whose fix text cannot be encoded as UTF-8.
            return;
        };
        fixes
    } else {
        Vec::new()
    };
    context.report_diagnostic(Diagnostic {
        range: Some(range),
        message: message(),
        fixes,
        ..Default::default()
    });
}

fn report_single_interpolation(
    context: &mut RuleContext<'_>,
    template: P<Node>,
    expression: P<Node>,
    literal: P<Node>,
) {
    let mut text = node_text(context, expression).to_string();
    let needs_parentheses = template.parent().is_some_and(|parent| {
        let parent_is_nullish_coalescing = parent.kind() == Kind::BinaryExpression
            && parent.as_binary_expression().operator_token.kind() == Kind::QuestionQuestionToken;
        let interpolation_is_logical = expression.kind() == Kind::BinaryExpression
            && matches!(
                expression.as_binary_expression().operator_token.kind(),
                Kind::BarBarToken | Kind::AmpersandAmpersandToken
            );
        parent_is_nullish_coalescing && interpolation_is_logical
            || ast::is_expression(parent)
                && ast::get_expression_precedence(expression)
                    <= ast::get_expression_precedence(parent)
    });
    if needs_parentheses {
        text = format!("({text})");
    }
    let fixes = vec![replace_node(context.source_file, template, text)];
    report_with_fixes(
        context,
        Range {
            pos: expression.pos().expect_source("parsed lint node") - 2,
            end: literal.pos().expect_source("parsed lint node") + 1,
        },
        Some(fixes),
    );
}

fn is_unnecessary_value_interpolation(
    context: &RuleContext<'_>,
    expression: P<Node>,
    prev_quasi_end: u32,
    literal: P<Node>,
) -> bool {
    let pos = literal.pos().expect_source("parsed lint node");
    if utils::has_comments_in_range(context.source_file, Range { pos: prev_quasi_end, end: pos })
        || utils::has_comments_in_range(
            context.source_file,
            Range { pos, end: trim_node_range(context.source_file, literal).pos },
        )
    {
        return false;
    }
    let expression = unwrap_literal_type(expression);
    if is_fixable_identifier(expression) {
        return true;
    }
    if ast::is_string_literal_like(expression) {
        let raw = raw_text(literal);
        return !(raw.starts_with('\n') || raw.starts_with("\r\n"))
            || !expression.text().chars().all(utils::is_str_white_space);
    }
    is_any_literal(expression) || expression.kind() == Kind::TemplateExpression
}

fn build_literal_replacement_text(
    context: &RuleContext<'_>,
    literal: P<Node>,
    next_is_curly: bool,
) -> (Option<String>, bool, bool) {
    let range = trim_node_range(context.source_file, literal);
    let text = match literal.kind() {
        Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral => {
            let bytes = &context.source_file.text().as_bytes()
                [range.pos as usize + 1..range.end as usize - 1];
            let Ok(text) = std::str::from_utf8(bytes) else {
                return (None, bytes.starts_with(b"{"), !bytes.is_empty());
            };
            text.to_string()
        }
        Kind::NumericLiteral => jsnum::from_string(&literal.text().replace('_', "")).to_string(),
        Kind::BigIntLiteral => jsnum::parse_pseudo_big_int(&literal.text().replace('_', "")),
        Kind::RegularExpressionLiteral => literal.text().replace('\\', "\\\\"),
        _ => node_text(context, literal).to_string(),
    };
    let mut text = escape_template_raw_text(&text);
    if next_is_curly && ends_with_unescaped_dollar_sign(&text) {
        text.insert(text.len() - 1, '\\');
    }
    let starts_with_curly = text.starts_with('{');
    let update_next = !text.is_empty();
    (Some(text), starts_with_curly, update_next)
}

fn is_trivial_interpolation(
    context: &RuleContext<'_>,
    spans: &[P<Node>],
    head: P<Node>,
    literal: P<Node>,
) -> bool {
    let pos = literal.pos().expect_source("parsed lint node");
    spans.len() == 1
        && head.text().is_empty()
        && literal.text().is_empty()
        && !utils::has_comments_in_range(
            context.source_file,
            Range { pos: head.end().expect_source("parsed lint node"), end: pos },
        )
        && !utils::has_comments_in_range(
            context.source_file,
            Range { pos, end: trim_node_range(context.source_file, literal).pos },
        )
}

fn is_enum_member_type(t: P<Type>) -> bool {
    utils::type_recurser(t, &mut |part| {
        part.symbol()
            .and_then(|symbol| symbol.value_declaration())
            .is_some_and(|declaration| declaration.kind() == Kind::EnumMember)
    })
}

fn check_template_spans(context: &mut RuleContext<'_>, spans: &[P<Node>], head: P<Node>) {
    let mut next_is_curly = false;
    for index in (0..spans.len()).rev() {
        let prev_quasi_end = if index == 0 { head.end() } else { spans[index - 1].end() }
            .expect_source("parsed lint node");
        let (expression, literal) = interpolation(spans[index]);
        if !is_unnecessary_value_interpolation(context, expression, prev_quasi_end, literal) {
            continue;
        }
        let raw = raw_text(literal);
        if !raw.is_empty() {
            next_is_curly = raw.starts_with('{');
        }
        let expression_range = trim_node_range(context.source_file, expression);
        let literal_start = trim_node_range(context.source_file, literal).pos;
        let mut fixes = vec![
            remove(Range { pos: prev_quasi_end - 2, end: expression_range.pos }),
            remove(Range { pos: expression_range.end, end: literal_start + 1 }),
        ];
        let mut fixes_are_utf8 = true;
        let unwrapped = unwrap_literal_type(expression);
        if ast::is_literal_expression(unwrapped) {
            let (replacement, starts_with_curly, should_update) =
                build_literal_replacement_text(context, unwrapped, next_is_curly);
            if should_update {
                next_is_curly = starts_with_curly;
            }
            if let Some(replacement) = replacement {
                fixes.push(replace_node(context.source_file, unwrapped, replacement));
            } else {
                fixes_are_utf8 = false;
            }
        } else if unwrapped.kind() == Kind::TemplateExpression {
            let range = trim_node_range(context.source_file, unwrapped);
            let template = unwrapped.as_template_expression();
            let last_span = template.template_spans.nodes().last().unwrap().as_template_span();
            if next_is_curly && ends_with_unescaped_dollar_sign(raw_text(last_span.literal)) {
                fixes.push(Fix {
                    range: Range { pos: range.end - 2, end: range.end - 2 },
                    text: "\\".into(),
                });
            }
            if !raw_text(template.head).is_empty() {
                next_is_curly = raw_text(template.head).starts_with('{');
            }
            fixes.extend([
                remove(Range { pos: range.pos, end: range.pos + 1 }),
                remove(Range { pos: range.end - 1, end: range.end }),
            ]);
        } else {
            next_is_curly = false;
        }
        let prev_raw =
            if index == 0 { raw_text(head) } else { raw_text(interpolation(spans[index - 1]).1) };
        if next_is_curly && ends_with_unescaped_dollar_sign(prev_raw) {
            fixes.push(Fix {
                range: Range { pos: prev_quasi_end - 3, end: prev_quasi_end - 2 },
                text: "\\$".into(),
            });
        }
        report_with_fixes(
            context,
            Range { pos: prev_quasi_end - 2, end: literal_start + 1 },
            fixes_are_utf8.then_some(fixes),
        );
    }
}

impl Rule for NoUnnecessaryTemplateExpression {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(listener, Listener::Enter(Kind::TemplateExpression | Kind::TemplateLiteralType))
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let (spans, head) = if node.kind() == Kind::TemplateExpression {
            if node.parent().is_some_and(|parent| parent.kind() == Kind::TaggedTemplateExpression) {
                return;
            }
            let template = node.as_template_expression();
            (template.template_spans.nodes(), template.head)
        } else {
            let template = node.as_template_literal_type_node();
            (template.template_spans.nodes(), template.head)
        };
        let (expression, literal) = interpolation(spans[0]);
        if is_trivial_interpolation(context, spans, head, literal) {
            let t = context.checker.get_type_at_location(expression);
            let parameter = t.flags().intersects(TypeFlags::TypeParameter);
            let constraint =
                if parameter { context.checker.get_base_constraint_of_type(t) } else { Some(t) };
            if let Some(constraint) = constraint
                && is_underlying_type_string(constraint)
                && (node.kind() == Kind::TemplateExpression
                    || !parameter && !is_enum_member_type(constraint))
            {
                report_single_interpolation(context, node, expression, literal);
                return;
            }
        }
        check_template_spans(context, spans, head);
    }
}
