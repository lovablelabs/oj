//! Rule execution contracts on native tsrs compiler objects.

use tsrs_ast::{Kind, Node, SourceFile};
use tsrs_checker::Checker;
use tsrs_compiler::Program;
use tsrs_core::P;

use crate::engine::Options;
use crate::protocol::{Diagnostic, Fix, Range, RuleMessage, Suggestion};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Listener {
    Enter(Kind),
    Exit(Kind),
    PatternEnter(Kind),
    PatternExit(Kind),
    ExpressionEnter(Kind),
    ExpressionExit(Kind),
}

pub struct RuleContext<'a> {
    pub source_file: P<SourceFile>,
    pub program: &'static Program,
    pub checker: &'a mut Checker,
    pub rule_name: &'a str,
    pub options: Options,
    pub diagnostics: &'a mut Vec<Diagnostic>,
}

impl RuleContext<'_> {
    pub fn report_range(&mut self, range: Range, message: RuleMessage) {
        self.report_diagnostic(Diagnostic { range: Some(range), message, ..Default::default() });
    }

    pub fn report_node(&mut self, node: P<Node>, message: RuleMessage) {
        self.report_range(trim_node_range(self.source_file, node), message);
    }

    pub fn report_diagnostic(&mut self, mut diagnostic: Diagnostic) {
        diagnostic.rule = Some(self.rule_name.into());
        diagnostic.file_path = Some(self.source_file.file_name().into());
        self.diagnostics.push(diagnostic);
    }

    pub fn report_node_with_fixes(
        &mut self,
        node: P<Node>,
        message: RuleMessage,
        fixes: impl FnOnce() -> Vec<Fix>,
    ) {
        let fixes = if self.options.fix { fixes() } else { Vec::new() };
        self.report_diagnostic(Diagnostic {
            range: Some(trim_node_range(self.source_file, node)),
            message,
            fixes,
            ..Default::default()
        });
    }

    pub fn report_node_with_suggestions(
        &mut self,
        node: P<Node>,
        message: RuleMessage,
        suggestions: impl FnOnce() -> Vec<Suggestion>,
    ) {
        let suggestions = if self.options.fix_suggestions { suggestions() } else { Vec::new() };
        self.report_diagnostic(Diagnostic {
            range: Some(trim_node_range(self.source_file, node)),
            message,
            suggestions,
            ..Default::default()
        });
    }
}

pub trait Rule {
    fn initialize(&mut self, _context: &mut RuleContext<'_>) {}

    /// Listener membership must remain stable after initialization for this file.
    fn listens_to(&self, listener: Listener) -> bool;
    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>);
}

pub fn trim_node_range(file: P<SourceFile>, node: P<Node>) -> Range {
    Range {
        pos: tsrs_scanner::get_range_of_token_at_position(
            file,
            node.pos().expect_source("lint nodes are parsed source nodes"),
        )
        .source_pos()
        .unwrap(),
        end: node.end().expect_source("lint nodes are parsed source nodes"),
    }
}

pub fn replace_node(file: P<SourceFile>, node: P<Node>, text: String) -> Fix {
    Fix { text, range: trim_node_range(file, node) }
}

pub fn visit_lint_nodes(file: P<SourceFile>, visitor: &mut impl FnMut(Listener, P<Node>)) {
    fn visit(node: P<Node>, pattern: bool, visitor: &mut impl FnMut(Listener, P<Node>)) {
        let kind = node.kind();
        visitor(Listener::Enter(kind), node);
        if pattern {
            visitor(Listener::PatternEnter(kind), node);
            match kind {
                Kind::ArrayLiteralExpression => {
                    for &element in node.elements() {
                        visit(element, true, visitor);
                    }
                }
                Kind::ObjectLiteralExpression => {
                    for &property in node.properties() {
                        visit(property, true, visitor);
                    }
                }
                Kind::SpreadElement | Kind::SpreadAssignment => {
                    visit(node.expression().expect("spread expression"), true, visitor);
                }
                Kind::PropertyAssignment => {
                    visit(node.initializer().expect("property value"), true, visitor);
                }
                _ => {
                    node.for_each_child(&mut |child| {
                        visit(child, false, visitor);
                        false
                    });
                }
            }
            visitor(Listener::PatternExit(kind), node);
        } else if matches!(kind, Kind::ArrayLiteralExpression | Kind::ObjectLiteralExpression) {
            visitor(Listener::ExpressionEnter(kind), node);
            node.for_each_child(&mut |child| {
                visit(child, false, visitor);
                false
            });
            visitor(Listener::ExpressionExit(kind), node);
        } else if tsrs_ast::is_assignment_expression(node, true) {
            let expression = node.as_binary_expression();
            visit(expression.left, true, visitor);
            visit(expression.operator_token, false, visitor);
            visit(expression.right(), false, visitor);
        } else {
            node.for_each_child(&mut |child| {
                visit(child, false, visitor);
                false
            });
        }
        visitor(Listener::Exit(kind), node);
    }
    file.as_node().for_each_child(&mut |child| {
        visit(child, false, visitor);
        false
    });
}
