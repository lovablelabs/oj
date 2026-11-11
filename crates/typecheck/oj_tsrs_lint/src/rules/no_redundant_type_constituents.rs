//! Port of tsgolint's `internal/rules/no_redundant_type_constituents/no_redundant_type_constituents.go`.

use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::{Type, TypeFlags};
use tsrs_core::P;
use tsrs_core::collections::OrderedMap;

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::utils::union_type_parts;

#[derive(Clone, Copy)]
enum PartSource {
    Node(P<Node>),
    Type(P<Type>),
}

#[derive(Clone, Copy)]
struct TypeFlagsWithNodeOrType {
    flags: TypeFlags,
    source: PartSource,
}

struct SeenUnionPart {
    flags: Vec<TypeFlagsWithNodeOrType>,
    type_node: P<Node>,
}

impl TypeFlagsWithNodeOrType {
    fn to_string(self, context: &mut RuleContext<'_>) -> String {
        match self.source {
            PartSource::Node(node) => {
                let text = match node.kind() {
                    Kind::AnyKeyword => "any",
                    Kind::BooleanKeyword => "boolean",
                    Kind::NeverKeyword => "never",
                    Kind::NumberKeyword => "number",
                    Kind::StringKeyword => "string",
                    Kind::UnknownKeyword => "unknown",
                    Kind::LiteralType => {
                        let literal = node.as_literal_type_node().literal;
                        match literal.kind() {
                            Kind::TemplateLiteralType | Kind::NoSubstitutionTemplateLiteral => {
                                "template literal type"
                            }
                            Kind::StringLiteral | Kind::NumericLiteral | Kind::BigIntLiteral => {
                                literal.text()
                            }
                            _ => "literal type",
                        }
                    }
                    _ => "literal type",
                };
                text.into()
            }
            PartSource::Type(t) => {
                let text = context.checker.type_to_string_exported(t);
                if t.flags().intersects(TypeFlags::StringLiteral) {
                    crate::go_quote::quote(&text)
                } else {
                    text
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum MessageId {
    ErrorTypeOverrides,
    LiteralOverridden,
    Overridden,
    Overrides,
    PrimitiveOverridden,
}

impl MessageId {
    fn message(self, type_name: &str, container: &str) -> RuleMessage {
        let (id, description) = match self {
            Self::ErrorTypeOverrides => (
                "errorTypeOverrides",
                format!(
                    "'{type_name}' is an 'error' type that acts as 'any' and overrides all other types in this {container} type."
                ),
            ),
            Self::LiteralOverridden => (
                "literalOverridden",
                format!("{type_name} is overridden by {container} in this union type."),
            ),
            Self::Overridden => (
                "overridden",
                format!("'{type_name}' is overridden by other types in this {container} type."),
            ),
            Self::Overrides => (
                "overrides",
                format!("'{type_name}' overrides all other types in this {container} type."),
            ),
            Self::PrimitiveOverridden => (
                "primitiveOverridden",
                format!("{container} is overridden by the {type_name} in this intersection type."),
            ),
        };
        RuleMessage { id: id.into(), description, ..Default::default() }
    }
}

fn get_type_node_type_part_flags(
    context: &mut RuleContext<'_>,
    node: P<Node>,
) -> Vec<TypeFlagsWithNodeOrType> {
    let node = ast::skip_parentheses(node);
    let mut flags = match node.kind() {
        Kind::AnyKeyword => TypeFlags::Any,
        Kind::BigIntKeyword => TypeFlags::BigInt,
        Kind::BooleanKeyword => TypeFlags::Boolean,
        Kind::NeverKeyword => TypeFlags::Never,
        Kind::NumberKeyword => TypeFlags::Number,
        Kind::StringKeyword => TypeFlags::String,
        Kind::UnknownKeyword => TypeFlags::Unknown,
        _ => TypeFlags::None,
    };
    if flags != TypeFlags::None {
        return vec![TypeFlagsWithNodeOrType { flags, source: PartSource::Node(node) }];
    }
    if node.kind() == Kind::LiteralType {
        flags = match node.as_literal_type_node().literal.kind() {
            Kind::BigIntLiteral => TypeFlags::BigIntLiteral,
            Kind::TrueKeyword | Kind::FalseKeyword => TypeFlags::BooleanLiteral,
            Kind::NumericLiteral => TypeFlags::NumberLiteral,
            Kind::StringLiteral => TypeFlags::StringLiteral,
            _ => TypeFlags::None,
        };
        if flags != TypeFlags::None {
            return vec![TypeFlagsWithNodeOrType { flags, source: PartSource::Node(node) }];
        }
    }
    if node.kind() == Kind::UnionType {
        let mut result = Vec::new();
        for &part in node.as_union_type_node().types().nodes() {
            result.extend(get_type_node_type_part_flags(context, part));
        }
        return result;
    }
    let t = context.checker.get_type_at_location(node);
    let parts = if t == context.checker.boolean_type {
        std::slice::from_ref(&t)
    } else {
        union_type_parts(&t)
    };
    let mut parts = parts.to_vec();
    let bigint_start =
        parts.partition_point(|part| part.flags().bits() < TypeFlags::BigIntLiteral.bits());
    let bigint_end = bigint_start
        + parts[bigint_start..].partition_point(|part| part.flags() == TypeFlags::BigIntLiteral);
    parts[bigint_start..bigint_end].sort_by_key(|part| part.id());
    parts
        .iter()
        .map(|&part| TypeFlagsWithNodeOrType {
            flags: part.flags(),
            source: PartSource::Type(part),
        })
        .collect()
}

fn check_bottom_and_top_types(
    context: &mut RuleContext<'_>,
    type_part: TypeFlagsWithNodeOrType,
    type_node: P<Node>,
    container: P<Node>,
) -> bool {
    let intersection = container.kind() == Kind::IntersectionType;
    let id = match type_part.flags {
        TypeFlags::Any => {
            let name = type_part.to_string(context);
            context.report_node(
                type_node,
                if name == "any" { MessageId::Overrides } else { MessageId::ErrorTypeOverrides }
                    .message(&name, if intersection { "intersection" } else { "union" }),
            );
            return true;
        }
        TypeFlags::Never => {
            if !intersection && container.parent().is_some_and(ast::is_function_like) {
                return false;
            }
            if intersection { MessageId::Overrides } else { MessageId::Overridden }
        }
        TypeFlags::Unknown => {
            if intersection {
                MessageId::Overridden
            } else {
                MessageId::Overrides
            }
        }
        _ => return false,
    };
    let name = if !intersection && type_part.flags == TypeFlags::Never {
        "never".into()
    } else {
        type_part.to_string(context)
    };
    context.report_node(
        type_node,
        id.message(&name, if intersection { "intersection" } else { "union" }),
    );
    true
}

const PRIMITIVES: [(TypeFlags, &str); 4] = [
    (TypeFlags::BigInt, "bigint"),
    (TypeFlags::Boolean, "boolean"),
    (TypeFlags::Number, "number"),
    (TypeFlags::String, "string"),
];

fn literal_index(flags: TypeFlags) -> Option<usize> {
    match flags {
        TypeFlags::BigIntLiteral => Some(0),
        TypeFlags::BooleanLiteral => Some(1),
        TypeFlags::NumberLiteral => Some(2),
        TypeFlags::StringLiteral | TypeFlags::TemplateLiteral => Some(3),
        _ => None,
    }
}

fn literals_string(context: &mut RuleContext<'_>, literals: &[TypeFlagsWithNodeOrType]) -> String {
    literals.iter().map(|&t| t.to_string(context)).collect::<Vec<_>>().join(" | ")
}

fn check_intersection(context: &mut RuleContext<'_>, node: P<Node>) {
    let mut seen_literal_types: [Vec<TypeFlagsWithNodeOrType>; 4] =
        std::array::from_fn(|_| Vec::new());
    let mut seen_primitive_types: [Vec<P<Node>>; 4] = std::array::from_fn(|_| Vec::new());
    let mut seen_union_types = Vec::new();
    for &type_node in node.as_intersection_type_node().types().nodes() {
        let type_part_flags = get_type_node_type_part_flags(context, type_node);
        if type_part_flags.len() >= 2 {
            seen_union_types.push(SeenUnionPart { flags: type_part_flags.clone(), type_node });
        }
        for type_part in type_part_flags {
            if check_bottom_and_top_types(context, type_part, type_node, node) {
                continue;
            }
            if seen_union_types.is_empty()
                && let Some(index) = literal_index(type_part.flags)
            {
                seen_literal_types[index].push(type_part);
            }
            if let Some(index) = PRIMITIVES.iter().position(|&(flag, _)| flag == type_part.flags) {
                seen_primitive_types[index].push(type_node);
            }
        }
    }
    if !seen_union_types.is_empty() && seen_primitive_types.iter().any(|types| !types.is_empty()) {
        let mut type_values_literal = String::new();
        for union_type in &seen_union_types {
            let mut primitive_name = "";
            for type_value in &union_type.flags {
                primitive_name = literal_index(type_value.flags)
                    .filter(|&index| !seen_primitive_types[index].is_empty())
                    .map_or("", |index| PRIMITIVES[index].1);
                if primitive_name.is_empty() {
                    break;
                }
            }
            if primitive_name.is_empty() {
                continue;
            }
            if type_values_literal.is_empty() {
                type_values_literal = literals_string(context, &union_type.flags);
            }
            context.report_node(
                union_type.type_node,
                MessageId::PrimitiveOverridden.message(primitive_name, &type_values_literal),
            );
        }
    }
    if !seen_union_types.is_empty() {
        return;
    }
    for (index, &(_, primitive_name)) in PRIMITIVES.iter().enumerate() {
        if seen_literal_types[index].is_empty() {
            continue;
        }
        let type_values_literal = literals_string(context, &seen_literal_types[index]);
        for &type_node in &seen_primitive_types[index] {
            context.report_node(
                type_node,
                MessageId::PrimitiveOverridden.message(&type_values_literal, primitive_name),
            );
        }
    }
}

fn check_union(context: &mut RuleContext<'_>, node: P<Node>) {
    let mut overridden_nodes: [OrderedMap<P<Node>, Vec<TypeFlagsWithNodeOrType>>; 4] =
        std::array::from_fn(|_| OrderedMap::default());
    let mut seen_primitive_type_flags = TypeFlags::None;
    for &type_node in node.as_union_type_node().types().nodes() {
        for type_part in get_type_node_type_part_flags(context, type_node) {
            if check_bottom_and_top_types(context, type_part, type_node, node) {
                continue;
            }
            if let Some(index) = literal_index(type_part.flags) {
                overridden_nodes[index].entry(type_node).or_default().push(type_part);
            }
            seen_primitive_type_flags |= type_part.flags
                & (TypeFlags::BigInt | TypeFlags::Boolean | TypeFlags::Number | TypeFlags::String);
        }
    }
    for (index, &(primitive_flag, primitive_name)) in PRIMITIVES.iter().enumerate() {
        if !seen_primitive_type_flags.intersects(primitive_flag) {
            continue;
        }
        for (&type_node, type_flags) in &overridden_nodes[index] {
            let text = literals_string(context, type_flags);
            context.report_node(
                type_node,
                MessageId::LiteralOverridden.message(&text, primitive_name),
            );
        }
    }
}

pub struct NoRedundantTypeConstituents;

impl Rule for NoRedundantTypeConstituents {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(listener, Listener::Enter(Kind::IntersectionType | Kind::UnionType))
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        if node.kind() == Kind::IntersectionType {
            check_intersection(context, node);
        } else {
            check_union(context, node);
        }
    }
}
