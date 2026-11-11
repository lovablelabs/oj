use rustc_hash::FxHashSet;
use tsrs_ast::{Kind, Node, SourceFile};
use tsrs_checker::{Checker, Type, TypeFlags};
use tsrs_core::{LanguageVariant, P, json::Value};

use crate::protocol::{Diagnostic, Fix, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, replace_node, trim_node_range};
use crate::utils::{
    get_constrained_type_at_location, intersection_type_parts, quote_single_string_literal,
    type_recurser, union_type_parts,
};

fn build_add_missing_cases_message() -> RuleMessage {
    RuleMessage {
        id: "addMissingCases".into(),
        description: "Add branches for missing cases.".into(),
        ..Default::default()
    }
}

fn build_dangerous_default_case_message() -> RuleMessage {
    RuleMessage {
        id: "dangerousDefaultCase".into(),
        description: "The switch statement is exhaustive, so the default case is unnecessary."
            .into(),
        ..Default::default()
    }
}

fn build_switch_is_not_exhaustive_message(missing_branches: &str) -> RuleMessage {
    RuleMessage {
        id: "switchIsNotExhaustive".into(),
        description: format!("Switch is not exhaustive. Cases not matched: {missing_branches}"),
        ..Default::default()
    }
}

struct SwitchMetadata {
    contains_non_literal_type: bool,
    default_case: Option<P<Node>>,
    default_case_comment: Option<Range>,
    missing_literal_branch_types: Vec<P<Type>>,
    symbol_name: String,
}

impl SwitchMetadata {
    fn has_default_case(&self) -> bool {
        self.default_case.is_some() || self.default_case_comment.is_some()
    }
}

fn is_literal_like_type(t: P<Type>) -> bool {
    t.flags().intersects(
        TypeFlags::Literal | TypeFlags::Undefined | TypeFlags::Null | TypeFlags::UniqueESSymbol,
    )
}

fn does_type_contain_non_literal_type(t: P<Type>) -> bool {
    union_type_parts(&t)
        .iter()
        .any(|part| intersection_type_parts(part).iter().all(|&t| !is_literal_like_type(t)))
}

fn get_comment_default_case(
    source_file: P<SourceFile>,
    node: P<Node>,
    pattern: Option<&regress::Regex>,
) -> Option<Range> {
    let block = node.as_switch_statement().case_block;
    let last = block.as_case_block().clauses.nodes().last()?;
    let start = last.end().expect_source("parsed case clause");
    let end = block.end().expect_source("parsed case block");
    let text = source_file.text();
    for comments in [
        tsrs_scanner::get_trailing_comment_ranges(text, start),
        tsrs_scanner::get_leading_comment_ranges(text, start),
    ] {
        for comment in comments {
            if comment.pos() >= end {
                break;
            }
            let mut content = &text[comment.pos() as usize..comment.end() as usize];
            if comment.kind == Kind::SingleLineCommentTrivia {
                content = content.strip_prefix("//").unwrap_or(content);
            } else if comment.kind == Kind::MultiLineCommentTrivia {
                content = content.strip_prefix("/*").unwrap_or(content);
                content = content.strip_suffix("*/").unwrap_or(content);
            }
            let content = content.trim();
            let matches = pattern.map_or_else(
                || content.eq_ignore_ascii_case("no default"),
                |pattern| pattern.find(content).is_some(),
            );
            if matches {
                return Some(Range { pos: comment.pos(), end: comment.end() });
            }
        }
    }
    None
}

fn get_switch_metadata(
    source_file: P<SourceFile>,
    checker: &mut Checker,
    node: P<Node>,
    pattern: Option<&regress::Regex>,
) -> SwitchMetadata {
    let statement = node.as_switch_statement();
    let cases = statement.case_block.as_case_block().clauses.nodes();
    let default_case = cases.iter().copied().find(|clause| clause.kind() == Kind::DefaultClause);
    let default_case_comment = if default_case.is_none() {
        get_comment_default_case(source_file, node, pattern)
    } else {
        None
    };
    let discriminant_type = get_constrained_type_at_location(checker, statement.expression);
    let symbol_name =
        discriminant_type.symbol().map_or_else(String::new, |symbol| symbol.name().to_string());
    let mut case_type_set = FxHashSet::default();
    let mut has_undefined_case = false;
    for &case in cases {
        if case.kind() == Kind::DefaultClause {
            continue;
        }
        let case_type = get_constrained_type_at_location(
            checker,
            case.as_case_or_default_clause().expression.unwrap(),
        );
        case_type_set.insert(case_type);
        if case_type.flags().intersects(TypeFlags::Undefined) {
            has_undefined_case = true;
        }
    }
    let contains_non_literal_type = does_type_contain_non_literal_type(discriminant_type);
    let mut missing_literal_branch_types = Vec::new();
    type_recurser(discriminant_type, &mut |t| {
        if case_type_set.contains(&t) || !is_literal_like_type(t) {
            return false;
        }
        if has_undefined_case && t.flags().intersects(TypeFlags::Undefined) {
            return false;
        }
        missing_literal_branch_types.push(t);
        false
    });
    SwitchMetadata {
        contains_non_literal_type,
        default_case,
        default_case_comment,
        missing_literal_branch_types,
        symbol_name,
    }
}

fn requires_quoting(text: &str) -> bool {
    !tsrs_scanner::is_identifier_text(text, LanguageVariant::Standard)
}

fn get_node_indent(source_file: P<SourceFile>, node: P<Node>) -> String {
    let trimmed = trim_node_range(source_file, node);
    let (_, column) =
        tsrs_scanner::get_ecma_line_and_utf16_character_of_position(source_file.get(), trimmed.pos);
    " ".repeat(column as usize)
}

fn build_case_test(
    checker: &mut Checker,
    missing_branch_type: P<Type>,
    symbol_name: &str,
) -> String {
    let missing_branch_name = missing_branch_type.symbol().map_or("", |symbol| symbol.name());
    let case_test = if missing_branch_type.flags().intersects(TypeFlags::ESSymbolLike) {
        missing_branch_name.to_string()
    } else {
        checker.type_to_string_exported(missing_branch_type)
    };
    if !symbol_name.is_empty() && requires_quoting(missing_branch_name) {
        return format!("{symbol_name}[{}]", quote_single_string_literal(missing_branch_name));
    }
    case_test
}

fn build_missing_case_line(
    checker: &mut Checker,
    missing_branch_type: Option<P<Type>>,
    symbol_name: &str,
) -> String {
    let Some(missing_branch_type) = missing_branch_type else {
        return "default: { throw new Error('default case') }".into();
    };
    let case_test = build_case_test(checker, missing_branch_type, symbol_name);
    let escaped_case = case_test.replace('\\', "\\\\").replace('\'', "\\'");
    format!("case {case_test}: {{ throw new Error('Not implemented yet: {escaped_case} case') }}")
}

fn build_missing_cases(
    checker: &mut Checker,
    missing_branch_types: &[Option<P<Type>>],
    symbol_name: &str,
) -> Vec<String> {
    missing_branch_types.iter().map(|&t| build_missing_case_line(checker, t, symbol_name)).collect()
}

fn apply_missing_cases(
    source_file: P<SourceFile>,
    node: P<Node>,
    default_case: Option<P<Node>>,
    missing_cases: &[String],
) -> Vec<Fix> {
    let statement = node.as_switch_statement();
    let cases = statement.case_block.as_case_block().clauses.nodes();
    let last_case = cases.last().copied();
    let case_indent = get_node_indent(source_file, last_case.unwrap_or(node));
    let fix_string = missing_cases
        .iter()
        .map(|code| format!("{case_indent}{code}"))
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(last_case) = last_case {
        if let Some(default_case) = default_case {
            let mut text = String::new();
            for code in missing_cases {
                text.push_str(code);
                text.push('\n');
                text.push_str(&case_indent);
            }
            let pos = trim_node_range(source_file, default_case).pos;
            return vec![Fix { text, range: Range { pos, end: pos } }];
        }
        let end = last_case.end().expect_source("parsed switch clause");
        return vec![Fix { text: format!("\n{fix_string}"), range: Range { pos: end, end } }];
    }
    vec![replace_node(
        source_file,
        statement.case_block,
        format!("{{\n{fix_string}\n{case_indent}}}"),
    )]
}

fn fix_switch(
    source_file: P<SourceFile>,
    checker: &mut Checker,
    node: P<Node>,
    missing_branch_types: &[Option<P<Type>>],
    default_case: Option<P<Node>>,
    symbol_name: &str,
) -> Vec<Fix> {
    let missing_cases = build_missing_cases(checker, missing_branch_types, symbol_name);
    apply_missing_cases(source_file, node, default_case, &missing_cases)
}

pub struct SwitchExhaustivenessCheck {
    allow_default_case_for_exhaustive_switch: bool,
    consider_default_exhaustive_for_unions: bool,
    require_default_for_non_union: bool,
    default_case_comment_pattern: Option<regress::Regex>,
}

struct CommentPatternScope {
    base_flags: u8,
    flags: u8,
    wrapped_flags: Option<u8>,
}

impl CommentPatternScope {
    fn new(flags: u8) -> Self {
        Self { base_flags: flags, flags, wrapped_flags: None }
    }

    fn close(&mut self, output: &mut String) {
        if self.wrapped_flags.take().is_some() {
            output.push(')');
        }
    }

    fn prepare(&mut self, output: &mut String) {
        if self.wrapped_flags == Some(self.flags)
            || self.wrapped_flags.is_none() && self.flags == self.base_flags
        {
            return;
        }
        self.close(output);
        if self.flags != self.base_flags {
            output.push_str("(?");
            for (flag, name) in [(1, 'i'), (2, 'm'), (4, 's')] {
                if self.flags & flag != 0 {
                    output.push(name);
                }
            }
            if self.flags != 7 {
                output.push('-');
                for (flag, name) in [(1, 'i'), (2, 'm'), (4, 's')] {
                    if self.flags & flag == 0 {
                        output.push(name);
                    }
                }
            }
            output.push(':');
            self.wrapped_flags = Some(self.flags);
        }
    }
}

fn comment_pattern_modifiers(pattern: &str, mut flags: u8) -> Option<(u8, u8, usize)> {
    let rest = pattern.strip_prefix("(?")?;
    let end = rest.find([')', ':'])?;
    let modifiers = &rest[..end];
    let mut enabled = true;
    let mut has_flag = false;
    for modifier in modifiers.chars() {
        let flag = match modifier {
            'i' => 1,
            'm' => 2,
            's' => 4,
            '-' if enabled => {
                enabled = false;
                continue;
            }
            _ => return None,
        };
        has_flag = true;
        if enabled {
            flags |= flag;
        } else {
            flags &= !flag;
        }
    }
    if !has_flag || modifiers.ends_with('-') {
        return None;
    }
    Some((flags, rest.as_bytes()[end], end + 3))
}

fn comment_pattern_quantifier_length(pattern: &str) -> Option<usize> {
    let rest = pattern.strip_prefix('{')?;
    let end = rest.find('}')?;
    let mut bounds = rest[..end].split(',');
    let minimum = bounds.next()?;
    if minimum.is_empty() || !minimum.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if bounds.next().is_some_and(|maximum| !maximum.bytes().all(|byte| byte.is_ascii_digit()))
        || bounds.next().is_some()
    {
        return None;
    }
    Some(end + 2)
}

fn normalize_comment_pattern(pattern: &str) -> String {
    let mut output = String::with_capacity(pattern.len());
    let mut scopes = vec![CommentPatternScope::new(0)];
    let mut position = 0;
    let bytes = pattern.as_bytes();
    while position < bytes.len() {
        let flags = scopes.last().unwrap().flags;
        if let Some((flags, delimiter, length)) =
            comment_pattern_modifiers(&pattern[position..], flags)
        {
            if delimiter == b')' {
                if bytes
                    .get(position + length)
                    .is_some_and(|byte| matches!(byte, b'*' | b'+' | b'?'))
                    || comment_pattern_quantifier_length(&pattern[position + length..]).is_some()
                {
                    return pattern.into();
                }
                scopes.last_mut().unwrap().flags = flags;
            } else {
                scopes.last_mut().unwrap().prepare(&mut output);
                output.push_str(&pattern[position..position + length]);
                scopes.push(CommentPatternScope::new(flags));
            }
            position += length;
            continue;
        }
        if pattern[position..].starts_with("(?#") {
            let Some(end) = pattern[position + 3..].find(')') else {
                return pattern.into();
            };
            position += end + 4;
            continue;
        }
        match bytes[position] {
            b'(' => {
                scopes.last_mut().unwrap().prepare(&mut output);
                output.push('(');
                scopes.push(CommentPatternScope::new(flags));
                position += 1;
            }
            b')' => {
                if scopes.len() == 1 {
                    return pattern.into();
                }
                scopes.pop().unwrap().close(&mut output);
                output.push(')');
                position += 1;
            }
            b'|' => {
                scopes.last_mut().unwrap().close(&mut output);
                output.push('|');
                position += 1;
            }
            b'*' | b'+' | b'?' => {
                output.push(char::from(bytes[position]));
                position += 1;
            }
            b'{' if comment_pattern_quantifier_length(&pattern[position..]).is_some() => {
                let end =
                    position + comment_pattern_quantifier_length(&pattern[position..]).unwrap();
                output.push_str(&pattern[position..end]);
                position = end;
            }
            b'{' | b'}' | b']' => {
                scopes.last_mut().unwrap().prepare(&mut output);
                output.push('\\');
                output.push(char::from(bytes[position]));
                position += 1;
            }
            _ => {
                scopes.last_mut().unwrap().prepare(&mut output);
                let start = position;
                if bytes[position] == b'[' {
                    position += 1;
                    while position < bytes.len() {
                        let byte = bytes[position];
                        position += 1;
                        if byte == b'\\' && position < bytes.len() {
                            position += 1;
                        } else if byte == b']' {
                            break;
                        }
                    }
                } else if bytes[position] == b'\\' {
                    position += 1;
                    if position < bytes.len() {
                        let escaped = pattern[position..].chars().next().unwrap();
                        position += escaped.len_utf8();
                        if matches!(escaped, 'p' | 'P' | 'u') && bytes.get(position) == Some(&b'{')
                        {
                            if let Some(end) = pattern[position..].find('}') {
                                position += end + 1;
                            }
                        } else if matches!(escaped, 'A' | 'G' | 'z' | 'Z') {
                            output.push_str(match escaped {
                                'A' | 'G' => "(?-m:^)",
                                'z' => "(?![\\s\\S])",
                                'Z' => "(?=\\n?(?![\\s\\S]))",
                                _ => unreachable!(),
                            });
                            continue;
                        } else if escaped == 'a' {
                            output.push_str("\\x07");
                            continue;
                        } else if escaped == 'e' {
                            output.push_str("\\x1b");
                            continue;
                        } else if escaped.is_alphabetic()
                            && !matches!(
                                escaped,
                                'b' | 'B'
                                    | 'd'
                                    | 'D'
                                    | 's'
                                    | 'S'
                                    | 'w'
                                    | 'W'
                                    | 'f'
                                    | 'n'
                                    | 'r'
                                    | 't'
                                    | 'v'
                                    | 'c'
                                    | 'x'
                                    | 'u'
                                    | 'p'
                                    | 'P'
                                    | 'k'
                            )
                        {
                            output.push(escaped);
                            continue;
                        }
                    }
                } else {
                    position += pattern[position..].chars().next().unwrap().len_utf8();
                }
                output.push_str(&pattern[start..position]);
            }
        }
    }
    if scopes.len() != 1 {
        return pattern.into();
    }
    scopes[0].close(&mut output);
    output
}

fn compile_comment_pattern(pattern: &str) -> Result<regress::Regex, String> {
    regress::Regex::with_flags(
        &normalize_comment_pattern(pattern),
        regress::Flags { unicode: true, ..Default::default() },
    )
    .map_err(|error| {
        format!("switch-exhaustiveness-check: invalid defaultCaseCommentPattern: {error}")
    })
}

impl SwitchExhaustivenessCheck {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("switch-exhaustiveness-check: options must be an object".into()),
        };
        let boolean = |name: &str, default| match object.and_then(|object| object.get(name)) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(format!("switch-exhaustiveness-check: {name} must be a boolean")),
        };
        let allow_default_case_for_exhaustive_switch =
            boolean("allowDefaultCaseForExhaustiveSwitch", true)?;
        let consider_default_exhaustive_for_unions =
            boolean("considerDefaultExhaustiveForUnions", false)?;
        let require_default_for_non_union = boolean("requireDefaultForNonUnion", false)?;
        let default_case_comment_pattern =
            match object.and_then(|object| object.get("defaultCaseCommentPattern")) {
                None | Some(Value::Null) => None,
                Some(Value::String(pattern)) => Some(compile_comment_pattern(pattern)?),
                _ => {
                    return Err(
                        "switch-exhaustiveness-check: defaultCaseCommentPattern must be a string"
                            .into(),
                    );
                }
            };
        Ok(Self {
            allow_default_case_for_exhaustive_switch,
            consider_default_exhaustive_for_unions,
            require_default_for_non_union,
            default_case_comment_pattern,
        })
    }

    fn report_missing_cases(
        context: &mut RuleContext<'_>,
        node: P<Node>,
        metadata: &SwitchMetadata,
        missing_branches: &str,
        types: &[Option<P<Type>>],
    ) {
        let suggestions = if context.options.fix_suggestions {
            vec![Suggestion {
                message: build_add_missing_cases_message(),
                fixes: fix_switch(
                    context.source_file,
                    context.checker,
                    node,
                    types,
                    metadata.default_case,
                    &metadata.symbol_name,
                ),
            }]
        } else {
            Vec::new()
        };
        context.report_diagnostic(Diagnostic {
            range: Some(trim_node_range(
                context.source_file,
                node.as_switch_statement().expression,
            )),
            message: build_switch_is_not_exhaustive_message(missing_branches),
            suggestions,
            ..Default::default()
        });
    }

    fn check_switch_exhaustive(
        &self,
        context: &mut RuleContext<'_>,
        node: P<Node>,
        metadata: &SwitchMetadata,
    ) {
        if self.consider_default_exhaustive_for_unions && metadata.has_default_case() {
            return;
        }
        if metadata.missing_literal_branch_types.is_empty() {
            return;
        }
        let missing_branches = metadata
            .missing_literal_branch_types
            .iter()
            .map(|&t| {
                if t.flags().intersects(TypeFlags::ESSymbolLike)
                    && let Some(symbol) = t.symbol()
                {
                    return format!("typeof {}", symbol.name());
                }
                context.checker.type_to_string_exported(t)
            })
            .collect::<Vec<_>>()
            .join(" | ");
        let types: Vec<_> =
            metadata.missing_literal_branch_types.iter().copied().map(Some).collect();
        Self::report_missing_cases(context, node, metadata, &missing_branches, &types);
    }

    fn check_switch_unnecessary_default_case(
        &self,
        context: &mut RuleContext<'_>,
        metadata: &SwitchMetadata,
    ) {
        if self.allow_default_case_for_exhaustive_switch {
            return;
        }
        if metadata.missing_literal_branch_types.is_empty()
            && !metadata.contains_non_literal_type
            && metadata.has_default_case()
        {
            if let Some(comment) = metadata.default_case_comment {
                context.report_range(comment, build_dangerous_default_case_message());
            } else if let Some(default_case) = metadata.default_case {
                context.report_node(default_case, build_dangerous_default_case_message());
            }
        }
    }

    fn check_switch_no_union_default_case(
        &self,
        context: &mut RuleContext<'_>,
        node: P<Node>,
        metadata: &SwitchMetadata,
    ) {
        if !self.require_default_for_non_union {
            return;
        }
        if metadata.contains_non_literal_type && !metadata.has_default_case() {
            Self::report_missing_cases(context, node, metadata, "default", &[None]);
        }
    }
}

impl Rule for SwitchExhaustivenessCheck {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::SwitchStatement)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let metadata = get_switch_metadata(
            context.source_file,
            context.checker,
            node,
            self.default_case_comment_pattern.as_ref(),
        );
        self.check_switch_exhaustive(context, node, &metadata);
        self.check_switch_unnecessary_default_case(context, &metadata);
        self.check_switch_no_union_default_case(context, node, &metadata);
    }
}
