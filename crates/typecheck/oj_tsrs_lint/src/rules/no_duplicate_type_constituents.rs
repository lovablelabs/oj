//! Port of tsgolint's `internal/rules/no_duplicate_type_constituents/no_duplicate_type_constituents.go`.

use rustc_hash::FxHashMap;
use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::{Type, TypeFlags};
use tsrs_core::{P, TextRange, json::Value};

use crate::protocol::{Diagnostic, Fix, LabeledRange, Range, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};

pub struct NoDuplicateTypeConstituents {
    ignore_intersections: bool,
    ignore_unions: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UnionOrIntersection {
    Union,
    Intersection,
}

impl UnionOrIntersection {
    fn kind(self) -> Kind {
        match self {
            Self::Union => Kind::UnionType,
            Self::Intersection => Kind::IntersectionType,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Union => "Union",
            Self::Intersection => "Intersection",
        }
    }
}

fn type_nodes(node: P<Node>) -> &'static [P<Node>] {
    if node.kind() == Kind::UnionType {
        node.as_union_type_node().types().nodes()
    } else {
        node.as_intersection_type_node().types().nodes()
    }
}

fn unwinded_parent_type(mut node: P<Node>, kind: Kind) -> Option<P<Node>> {
    loop {
        node = node.parent()?;
        if node.kind() == kind {
            return Some(node);
        }
        if node.kind() != Kind::ParenthesizedType {
            return None;
        }
    }
}

fn remove_range(range: Range) -> Fix {
    Fix { range, text: String::new() }
}

fn source_range(range: TextRange) -> Range {
    Range { pos: range.source_pos().unwrap(), end: range.source_end().unwrap() }
}

fn fixes(context: &RuleContext<'_>, constituent: P<Node>, kind: UnionOrIntersection) -> Vec<Fix> {
    let parent = unwinded_parent_type(constituent, kind.kind()).unwrap();
    let mut scanner = tsrs_scanner::get_scanner_for_source_file(
        context.source_file,
        parent.pos().expect_source("parsed lint node"),
    );
    let mut found_before = false;
    let mut previous_start = 0;
    let mut bracket_before_tokens = Vec::new();
    while scanner.token_start() < constituent.pos().expect_source("parsed lint node") {
        if matches!(scanner.token(), Kind::AmpersandToken | Kind::BarToken) {
            found_before = true;
            previous_start = scanner.token_start();
            bracket_before_tokens.clear();
        } else if scanner.token() == Kind::OpenParenToken {
            bracket_before_tokens.push(source_range(scanner.token_range()));
        }
        scanner.scan();
    }
    let mut fixes = vec![remove_range(trim_node_range(context.source_file, constituent))];
    if found_before {
        fixes.push(remove_range(Range { pos: previous_start, end: previous_start + 1 }));
        for &range in &bracket_before_tokens {
            fixes.push(remove_range(range));
        }
        scanner.reset_pos(constituent.end().expect_source("parsed lint node"));
        for _ in &bracket_before_tokens {
            scanner.scan();
            assert_eq!(
                scanner.token(),
                Kind::CloseParenToken,
                "expected next scanned token to be ')'"
            );
            fixes.push(remove_range(source_range(scanner.token_range())));
        }
    } else {
        scanner.reset_pos(constituent.end().expect_source("parsed lint node"));
        let mut closing_parens_count = 0;
        loop {
            scanner.scan();
            assert!(
                scanner.token_start() < parent.end().expect_source("parsed lint node"),
                "couldn't find '&' or '|' token"
            );
            if matches!(scanner.token(), Kind::AmpersandToken | Kind::BarToken) {
                fixes.push(remove_range(source_range(scanner.token_range())));
                break;
            }
            assert_eq!(
                scanner.token(),
                Kind::CloseParenToken,
                "expected next scanned token to be ')'"
            );
            closing_parens_count += 1;
            fixes.push(remove_range(source_range(scanner.token_range())));
        }
        let mut opening_parens = Vec::with_capacity(closing_parens_count);
        scanner.reset_pos(parent.pos().expect_source("parsed lint node"));
        for _ in 0..closing_parens_count {
            scanner.scan();
            if scanner.token() == Kind::OpenParenToken {
                if opening_parens.len() < closing_parens_count {
                    opening_parens.push(source_range(scanner.token_range()));
                }
            } else {
                opening_parens.clear();
            }
            if scanner.token_start() == constituent.pos().expect_source("parsed lint node") {
                assert_eq!(
                    opening_parens.len(),
                    closing_parens_count,
                    "expected to find opening parens"
                );
                break;
            }
            for &range in &opening_parens {
                fixes.push(remove_range(range));
            }
        }
    }
    fixes
}

fn report(
    context: &mut RuleContext<'_>,
    with_fix: bool,
    kind: UnionOrIntersection,
    message: RuleMessage,
    constituent: P<Node>,
    first: Option<P<Node>>,
) {
    let labeled_ranges = first.map_or_else(Vec::new, |first| {
        let range = trim_node_range(context.source_file, first);
        let text = &context.source_file.text()[range.pos as usize..range.end as usize];
        vec![LabeledRange { label: format!("Type '{text}' is first declared here."), range }]
    });
    let fixes = if with_fix && context.options.fix {
        fixes(context, constituent, kind)
    } else {
        Vec::new()
    };
    context.report_diagnostic(Diagnostic {
        range: Some(trim_node_range(context.source_file, constituent)),
        message,
        labeled_ranges,
        fixes,
        ..Default::default()
    });
}

fn check_duplicate_recursively(
    context: &mut RuleContext<'_>,
    with_fix: bool,
    kind: UnionOrIntersection,
    constituent: P<Node>,
    cached_types: &mut FxHashMap<P<Type>, P<Node>>,
    optional_parameter: bool,
) -> bool {
    let t = context.checker.get_type_at_location(constituent);
    if t.flags().intersects(TypeFlags::Intrinsic)
        && t.as_intrinsic_type().intrinsic_name() == "error"
    {
        return false;
    }
    if let Some(&previous) = cached_types.get(&t) {
        let text = &context.source_file.text()[previous.pos().expect_source("parsed lint node")
            as usize
            ..previous.end().expect_source("parsed lint node") as usize];
        let message = RuleMessage {
            id: "duplicate".into(),
            description: format!("{} type constituent is duplicated with {text}.", kind.name()),
            ..Default::default()
        };
        report(context, with_fix, kind, message, constituent, Some(previous));
        return true;
    }
    if optional_parameter && t.flags().intersects(TypeFlags::Undefined) {
        report(
            context,
            true,
            UnionOrIntersection::Union,
            RuleMessage {
                id: "unnecessary".into(),
                description: "Explicit undefined is unnecessary on an optional parameter.".into(),
                ..Default::default()
            },
            constituent,
            None,
        );
    }
    cached_types.insert(t, constituent);
    let inner = ast::skip_type_parentheses(constituent);
    if inner.kind() != kind.kind() {
        return false;
    }
    let types = type_nodes(inner);
    let mut all_previous_removed = true;
    for (index, &constituent) in types.iter().enumerate() {
        if !check_duplicate_recursively(
            context,
            index < types.len() - 1 || !all_previous_removed,
            kind,
            constituent,
            cached_types,
            optional_parameter,
        ) {
            all_previous_removed = false;
        }
    }
    false
}

impl NoDuplicateTypeConstituents {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("no-duplicate-type-constituents: options must be an object".into()),
        };
        let boolean = |name: &str| match object.and_then(|object| object.get(name)) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(format!("no-duplicate-type-constituents: {name} must be a boolean")),
        };
        Ok(Self {
            ignore_intersections: boolean("ignoreIntersections")?,
            ignore_unions: boolean("ignoreUnions")?,
        })
    }
}

impl Rule for NoDuplicateTypeConstituents {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::IntersectionType) && !self.ignore_intersections
            || listener == Listener::Enter(Kind::UnionType) && !self.ignore_unions
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        if unwinded_parent_type(node, node.kind()).is_some() {
            return;
        }
        let kind = if node.kind() == Kind::IntersectionType {
            UnionOrIntersection::Intersection
        } else {
            UnionOrIntersection::Union
        };
        let optional_parameter = kind == UnionOrIntersection::Union
            && node.parent().is_some_and(|parent| {
                parent.kind() == Kind::Parameter
                    && parent.as_parameter_declaration().question_token().is_some()
                    && !parent
                        .parent()
                        .is_some_and(|owner| ast::is_parameter_property_declaration(parent, owner))
            });
        let mut cached_types = FxHashMap::default();
        for &constituent in type_nodes(node) {
            check_duplicate_recursively(
                context,
                true,
                kind,
                constituent,
                &mut cached_types,
                optional_parameter,
            );
        }
    }
}
