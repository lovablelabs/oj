//! Port of tsgolint's `internal/rules/no_unnecessary_qualifier/no_unnecessary_qualifier.go`.

use tsrs_ast::{Kind, Node, Symbol, SymbolFlags};
use tsrs_core::P;

use crate::protocol::{Fix, Range, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};

#[derive(Default)]
pub struct NoUnnecessaryQualifier {
    namespaces_in_scope: Vec<P<Node>>,
    current_failed_namespace_expression: Option<P<Node>>,
}

fn is_entity_name_expression(node: P<Node>) -> bool {
    match node.kind() {
        Kind::Identifier => true,
        Kind::PropertyAccessExpression => {
            is_entity_name_expression(node.as_property_access_expression().expression)
        }
        _ => false,
    }
}

impl NoUnnecessaryQualifier {
    fn symbol_is_namespace_in_scope(
        &self,
        context: &mut RuleContext<'_>,
        symbol: P<Symbol>,
    ) -> bool {
        if symbol
            .declarations()
            .iter()
            .any(|declaration| self.namespaces_in_scope.contains(declaration))
        {
            return true;
        }
        if symbol.flags().intersects(SymbolFlags::Alias) {
            let target = context.checker.get_aliased_symbol(symbol);
            return self.symbol_is_namespace_in_scope(context, target);
        }
        false
    }

    fn qualifier_is_unnecessary(
        &self,
        context: &mut RuleContext<'_>,
        qualifier: P<Node>,
        name: P<Node>,
    ) -> bool {
        let Some(namespace) = context.checker.get_symbol_at_location_exported(qualifier) else {
            return false;
        };
        if !self.symbol_is_namespace_in_scope(context, namespace) {
            return false;
        }
        let Some(accessed) = context.checker.get_symbol_at_location_exported(name) else {
            return false;
        };
        let in_scope = context
            .checker
            .get_symbols_in_scope_exported(qualifier, accessed.flags())
            .into_iter()
            .find(|symbol| symbol.name() == name.text());
        in_scope
            .is_some_and(|symbol| accessed == context.checker.get_export_symbol_of_symbol(symbol))
    }

    fn visit_namespace_access(
        &mut self,
        context: &mut RuleContext<'_>,
        node: P<Node>,
        qualifier: P<Node>,
        name: P<Node>,
    ) {
        if self.current_failed_namespace_expression.is_some()
            || !self.qualifier_is_unnecessary(context, qualifier, name)
        {
            return;
        }
        self.current_failed_namespace_expression = Some(node);
        let file = context.source_file;
        context.report_node_with_fixes(
            qualifier,
            RuleMessage {
                id: "unnecessaryQualifier".into(),
                description: format!(
                    "Qualifier is unnecessary since '{}' is in scope.",
                    name.text()
                ),
                ..Default::default()
            },
            || {
                vec![Fix {
                    range: Range {
                        pos: trim_node_range(file, qualifier).pos,
                        end: trim_node_range(file, name).pos,
                    },
                    text: String::new(),
                }]
            },
        );
    }
}

impl Rule for NoUnnecessaryQualifier {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::EnumDeclaration
                    | Kind::ModuleBlock
                    | Kind::PropertyAccessExpression
                    | Kind::QualifiedName
            ) | Listener::Exit(
                Kind::EnumDeclaration
                    | Kind::ModuleBlock
                    | Kind::PropertyAccessExpression
                    | Kind::QualifiedName
            )
        )
    }

    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match listener {
            Listener::Enter(Kind::EnumDeclaration) => self.namespaces_in_scope.push(node),
            Listener::Exit(Kind::EnumDeclaration) => {
                self.namespaces_in_scope.pop();
            }
            Listener::Enter(Kind::ModuleBlock) => {
                if let Some(parent) =
                    node.parent().filter(|parent| parent.kind() == Kind::ModuleDeclaration)
                {
                    self.namespaces_in_scope.push(parent);
                }
            }
            Listener::Exit(Kind::ModuleBlock) => {
                if node.parent().is_some_and(|parent| parent.kind() == Kind::ModuleDeclaration) {
                    self.namespaces_in_scope.pop();
                }
            }
            Listener::Enter(Kind::PropertyAccessExpression) => {
                let access = node.as_property_access_expression();
                if let Some(name) = node.name()
                    && is_entity_name_expression(access.expression)
                {
                    self.visit_namespace_access(context, node, access.expression, name);
                }
            }
            Listener::Enter(Kind::QualifiedName) => {
                let name = node.as_qualified_name();
                self.visit_namespace_access(context, node, name.left, name.right);
            }
            Listener::Exit(Kind::PropertyAccessExpression | Kind::QualifiedName) => {
                if self.current_failed_namespace_expression == Some(node) {
                    self.current_failed_namespace_expression = None;
                }
            }
            _ => unreachable!("no-unnecessary-qualifier listener"),
        }
    }
}
