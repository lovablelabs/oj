//! Port of tsgolint's `internal/rules/consistent_type_exports/consistent_type_exports.go`.

use rustc_hash::FxHashSet;

use tsrs_ast::{self as ast, Kind, Node, SourceFile, Symbol, SymbolFlags};
use tsrs_checker::Checker;
use tsrs_core::P;
use tsrs_core::json::Value;

use crate::protocol::{Diagnostic, Fix, LabeledRange, Range, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};

fn build_type_over_value_message() -> RuleMessage {
    RuleMessage {
        id: "typeOverValue".into(),
        description: "All exports in the declaration are only used as types.".into(),
        help: "Use `export type`.".into(),
    }
}

fn build_single_export_is_type_message(name: &str) -> RuleMessage {
    RuleMessage {
        id: "singleExportIsType".into(),
        description: format!(
            "Type export {name} is not a value and should be exported using `export type`."
        ),
        help: format!("Try adding the `type` keyword: `type {name}`"),
    }
}

fn build_multiple_exports_are_types_message(names: &str) -> RuleMessage {
    RuleMessage {
        id: "multipleExportsAreTypes".into(),
        description: format!(
            "Type exports {names} are not values and should be exported using `export type`."
        ),
        help: String::new(),
    }
}

struct AnalyzedNamedExport {
    node: P<Node>,
    type_texts: Vec<&'static str>,
    type_based_nodes: Vec<P<Node>>,
    type_based_names: Vec<&'static str>,
    value_texts: Vec<&'static str>,
    module_source: &'static str,
}

fn is_symbol_type_based(checker: &mut Checker, mut symbol: Option<P<Symbol>>) -> (bool, bool) {
    let mut visited = FxHashSet::default();
    while let Some(current) = symbol {
        if !visited.insert(current) {
            break;
        }
        if current
            .declarations()
            .iter()
            .any(|&node| ast::is_type_only_import_or_export_declaration(node))
        {
            return (true, true);
        }
        if current.flags().intersects(SymbolFlags::Value) {
            return (false, true);
        }
        if current.flags().intersects(SymbolFlags::Alias) {
            symbol = checker.get_immediate_aliased_symbol(current);
            if symbol.is_none() {
                return (false, false);
            }
            continue;
        }
        return (true, true);
    }
    (false, false)
}

fn get_node_text(file: P<SourceFile>, node: P<Node>) -> &'static str {
    let range = trim_node_range(file, node);
    &file.text()[range.pos as usize..range.end as usize]
}

fn strip_leading_type_keyword(text: &str) -> &str {
    if !text.starts_with("type") {
        return text;
    }
    if text.len() == 4 {
        return "";
    }
    let next = char::from(text.as_bytes()[4]);
    if !next.is_whitespace() && next != '/' {
        return text;
    }
    text[4..].trim_start_matches([' ', '\t', '\r', '\n'])
}

fn get_export_specifier_text(file: P<SourceFile>, node: P<Node>) -> &'static str {
    let specifier = node.as_export_specifier();
    let text = get_node_text(file, node).trim();
    if specifier.is_type_only {
        return strip_leading_type_keyword(text);
    }
    let local = specifier.property_name.unwrap_or(specifier.name);
    let local_text = get_node_text(file, local).trim();
    let exported_text = get_node_text(file, specifier.name).trim();
    if local_text == exported_text {
        local_text
    } else {
        tsrs_core::alloc_str(&format!("{local_text} as {exported_text}"))
    }
}

fn get_export_keyword_range(file: P<SourceFile>, node: P<Node>) -> Range {
    let mut scanner =
        tsrs_scanner::get_scanner_for_source_file(file, node.pos().expect_source("parsed export"));
    loop {
        let range = scanner.token_range();
        if scanner.token() == Kind::ExportKeyword {
            return Range { pos: range.source_pos().unwrap(), end: range.source_end().unwrap() };
        }
        if scanner.token() == Kind::EndOfFile
            || range.source_pos().unwrap() >= node.end().expect_source("parsed export")
        {
            break;
        }
        scanner.scan();
    }
    trim_node_range(file, node)
}

fn join_word_list(words: &[&str]) -> String {
    match words {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [one, two] => format!("{one} and {two}"),
        _ => format!("{}, and {}", words[..words.len() - 1].join(", "), words.last().unwrap()),
    }
}

fn build_named_export_statement(type_only: bool, specifiers: &[&str], source: &str) -> String {
    let mut statement =
        if type_only { "export type { ".to_owned() } else { "export { ".to_owned() };
    statement.push_str(&specifiers.join(", "));
    statement.push_str(" }");
    if !source.is_empty() {
        statement.push_str(" from ");
        statement.push_str(source);
    }
    statement.push(';');
    statement
}

pub struct ConsistentTypeExports {
    fix_mixed_exports_with_inline_type_specifier: bool,
}

impl ConsistentTypeExports {
    pub fn new(options: &Value) -> Result<Self, String> {
        let inline = match options {
            Value::Null => false,
            Value::Object(object) => match object.get("fixMixedExportsWithInlineTypeSpecifier") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(value)) => *value,
                _ => return Err("consistent-type-exports: fixMixedExportsWithInlineTypeSpecifier must be a boolean".into()),
            },
            _ => return Err("consistent-type-exports: options must be an object".into()),
        };
        Ok(Self { fix_mixed_exports_with_inline_type_specifier: inline })
    }

    fn check_star_export(node: P<Node>, context: &mut RuleContext<'_>) {
        let declaration = node.as_export_declaration();
        let Some(module) = declaration.module_specifier else {
            return;
        };
        if declaration.is_type_only
            || declaration
                .export_clause
                .is_some_and(|clause| clause.kind() != Kind::NamespaceExport)
        {
            return;
        }
        let Some(symbol) = context.checker.get_symbol_at_location_exported(module) else {
            return;
        };
        let t = context.checker.get_type_of_symbol(symbol);
        if context
            .checker
            .get_properties_of_type(t)
            .iter()
            .any(|property| context.checker.get_property_of_type(t, property.name()).is_some())
        {
            return;
        }
        let mut diagnostic = Diagnostic {
            range: Some(get_export_keyword_range(context.source_file, node)),
            message: build_type_over_value_message(),
            ..Default::default()
        };
        if context.options.fix {
            let mut scanner = tsrs_scanner::get_scanner_for_source_file(
                context.source_file,
                node.pos().expect_source("parsed export"),
            );
            loop {
                let range = scanner.token_range();
                if scanner.token() == Kind::AsteriskToken {
                    let pos = range.source_pos().unwrap();
                    diagnostic
                        .fixes
                        .push(Fix { text: "type ".into(), range: Range { pos, end: pos } });
                    break;
                }
                if scanner.token() == Kind::EndOfFile
                    || range.source_pos().unwrap() >= node.end().expect_source("parsed export")
                {
                    break;
                }
                scanner.scan();
            }
        }
        context.report_diagnostic(diagnostic);
    }

    fn analyze_named_export(
        node: P<Node>,
        context: &mut RuleContext<'_>,
    ) -> Option<AnalyzedNamedExport> {
        let declaration = node.as_export_declaration();
        let clause = declaration.export_clause?;
        if clause.kind() != Kind::NamedExports || declaration.is_type_only {
            return None;
        }
        let mut report = AnalyzedNamedExport {
            node,
            type_texts: Vec::with_capacity(2),
            type_based_nodes: Vec::with_capacity(2),
            type_based_names: Vec::with_capacity(2),
            value_texts: Vec::with_capacity(2),
            module_source: declaration
                .module_specifier
                .map_or("", |module| get_node_text(context.source_file, module).trim()),
        };
        for &node in clause.as_named_exports().elements.nodes() {
            let specifier = node.as_export_specifier();
            let text = get_export_specifier_text(context.source_file, node);
            if specifier.is_type_only {
                report.type_texts.push(text);
                continue;
            }
            let name = specifier.property_name.unwrap_or(specifier.name);
            let symbol = context.checker.get_symbol_at_location_exported(name);
            let (is_type, resolved) = is_symbol_type_based(context.checker, symbol);
            if !resolved {
                continue;
            }
            if is_type {
                report.type_texts.push(text);
                report.type_based_nodes.push(node);
                report.type_based_names.push(text);
            } else {
                report.value_texts.push(text);
            }
        }
        Some(report)
    }

    fn check_named_export(&self, report: AnalyzedNamedExport, context: &mut RuleContext<'_>) {
        if report.type_based_nodes.is_empty() {
            return;
        }
        let export_range = get_export_keyword_range(context.source_file, report.node);
        if report.value_texts.is_empty() {
            let mut diagnostic = Diagnostic {
                range: Some(export_range),
                message: build_type_over_value_message(),
                ..Default::default()
            };
            if context.options.fix {
                diagnostic.fixes.push(Fix {
                    range: trim_node_range(context.source_file, report.node),
                    text: build_named_export_statement(
                        true,
                        &report.type_texts,
                        report.module_source,
                    ),
                });
            }
            context.report_diagnostic(diagnostic);
            return;
        }
        let mut diagnostic = if report.type_based_names.len() == 1 {
            Diagnostic {
                range: Some(trim_node_range(context.source_file, report.type_based_nodes[0])),
                message: build_single_export_is_type_message(report.type_based_names[0]),
                ..Default::default()
            }
        } else {
            Diagnostic {
                range: Some(Range { pos: 0, end: 0 }),
                message: build_multiple_exports_are_types_message(&join_word_list(
                    &report.type_based_names,
                )),
                labeled_ranges: report
                    .type_based_nodes
                    .iter()
                    .zip(&report.type_based_names)
                    .map(|(&node, name)| LabeledRange {
                        label: format!("{name} is a type export, try `type {name}`"),
                        range: trim_node_range(context.source_file, node),
                    })
                    .collect(),
                ..Default::default()
            }
        };
        if context.options.fix {
            if self.fix_mixed_exports_with_inline_type_specifier {
                for node in report.type_based_nodes {
                    let pos = trim_node_range(context.source_file, node).pos;
                    diagnostic
                        .fixes
                        .push(Fix { text: "type ".into(), range: Range { pos, end: pos } });
                }
            } else {
                diagnostic.fixes.push(Fix {
                    range: trim_node_range(context.source_file, report.node),
                    text: format!(
                        "{}\n{}",
                        build_named_export_statement(
                            true,
                            &report.type_texts,
                            report.module_source
                        ),
                        build_named_export_statement(
                            false,
                            &report.value_texts,
                            report.module_source
                        )
                    ),
                });
            }
        }
        context.report_diagnostic(diagnostic);
    }
}

impl Rule for ConsistentTypeExports {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::ExportDeclaration)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let clause = node.as_export_declaration().export_clause;
        if clause.is_none_or(|clause| clause.kind() == Kind::NamespaceExport) {
            Self::check_star_export(node, context);
            return;
        }
        if let Some(report) = Self::analyze_named_export(node, context) {
            self.check_named_export(report, context);
        }
    }
}
