use bitflags::bitflags;
use tsrs_core::P;

use crate::{Kind, Node, is_optional_chain};

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum OperatorPrecedence {
    // Invalid is lower than all valid precedences. Returning it will cause binary expression parsing to stop.
    Invalid,
    // Expression:
    //     AssignmentExpression
    //     Expression `,` AssignmentExpression
    Comma = 1,
    // NOTE: `Spread` is higher than `Comma` due to how it is parsed in |ElementList|
    // SpreadElement:
    //     `...` AssignmentExpression
    Spread,
    // AssignmentExpression: YieldExpression
    Yield,
    // AssignmentExpression: LeftHandSideExpression `=` AssignmentExpression
    // AssignmentExpression: LeftHandSideExpression AssignmentOperator AssignmentExpression
    Assignment,
    // NOTE: `Conditional` is considered higher than `Assignment` here, but in reality they have
    //       the same precedence.
    Conditional,
    // LogicalORExpression
    LogicalOR,
    // LogicalANDExpression
    LogicalAND,
    // BitwiseORExpression
    BitwiseOR,
    // BitwiseXORExpression
    BitwiseXOR,
    // BitwiseANDExpression
    BitwiseAND,
    // EqualityExpression
    Equality,
    // RelationalExpression
    Relational,
    // ShiftExpression
    Shift,
    // AdditiveExpression
    Additive,
    // MultiplicativeExpression
    Multiplicative,
    // ExponentiationExpression
    Exponentiation,
    // UnaryExpression
    Unary,
    // UpdateExpression
    Update,
    // LeftHandSideExpression: NewExpression
    LeftHandSide,
    // LeftHandSideExpression: OptionalExpression
    OptionalChain,
    // LeftHandSideExpression: CallExpression, MemberExpression
    Member,
    // PrimaryExpression
    Primary,
    // PrimaryExpression:
    //     CoverParenthesizedExpressionAndArrowParameterList
    Parentheses,
}

impl OperatorPrecedence {
    pub const LOWEST: OperatorPrecedence = OperatorPrecedence::Comma;
    pub const HIGHEST: OperatorPrecedence = OperatorPrecedence::Parentheses;
    pub const DISALLOW_COMMA: OperatorPrecedence = OperatorPrecedence::Yield;
    // ShortCircuitExpression:
    //     LogicalORExpression
    //     CoalesceExpression
    pub const COALESCE: OperatorPrecedence = OperatorPrecedence::LogicalOR;
}

fn get_operator(expression: P<Node>) -> Kind {
    match expression.kind() {
        Kind::BinaryExpression => expression.as_binary_expression().operator_token.kind(),
        Kind::PrefixUnaryExpression => expression.as_prefix_unary_expression().operator,
        Kind::PostfixUnaryExpression => expression.as_postfix_unary_expression().operator,
        _ => expression.kind(),
    }
}

// Gets the precedence of an expression
pub fn get_expression_precedence(expression: P<Node>) -> OperatorPrecedence {
    let operator = get_operator(expression);
    let mut flags = OperatorPrecedenceFlags::None;
    if expression.kind() == Kind::NewExpression && expression.argument_list().is_none() {
        flags = OperatorPrecedenceFlags::NewWithoutArguments;
    } else if is_optional_chain(expression) {
        flags = OperatorPrecedenceFlags::OptionalChain;
    }
    get_operator_precedence(expression.kind(), operator, flags)
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct OperatorPrecedenceFlags: u32 {
        const None = 0;
        const NewWithoutArguments = 1 << 0;
        const OptionalChain = 1 << 1;
    }
}

// Gets the precedence of an operator
fn get_operator_precedence(
    node_kind: Kind,
    operator_kind: Kind,
    flags: OperatorPrecedenceFlags,
) -> OperatorPrecedence {
    match node_kind {
        Kind::SpreadElement => OperatorPrecedence::Spread,
        Kind::YieldExpression => OperatorPrecedence::Yield,
        // !!! By necessity, this differs from the old compiler to better align with ParenthesizerRules. consider backporting
        Kind::ArrowFunction => OperatorPrecedence::Assignment,
        Kind::ConditionalExpression => OperatorPrecedence::Conditional,
        Kind::BinaryExpression => match operator_kind {
            Kind::CommaToken => OperatorPrecedence::Comma,
            Kind::EqualsToken
            | Kind::PlusEqualsToken
            | Kind::MinusEqualsToken
            | Kind::AsteriskAsteriskEqualsToken
            | Kind::AsteriskEqualsToken
            | Kind::SlashEqualsToken
            | Kind::PercentEqualsToken
            | Kind::LessThanLessThanEqualsToken
            | Kind::GreaterThanGreaterThanEqualsToken
            | Kind::GreaterThanGreaterThanGreaterThanEqualsToken
            | Kind::AmpersandEqualsToken
            | Kind::CaretEqualsToken
            | Kind::BarEqualsToken
            | Kind::BarBarEqualsToken
            | Kind::AmpersandAmpersandEqualsToken
            | Kind::QuestionQuestionEqualsToken => OperatorPrecedence::Assignment,
            _ => get_binary_operator_precedence(operator_kind),
        },
        // TODO: Should prefix `++` and `--` be moved to the `Update` precedence?
        Kind::TypeAssertionExpression
        | Kind::NonNullExpression
        | Kind::PrefixUnaryExpression
        | Kind::TypeOfExpression
        | Kind::VoidExpression
        | Kind::DeleteExpression
        | Kind::AwaitExpression => OperatorPrecedence::Unary,
        Kind::PostfixUnaryExpression => OperatorPrecedence::Update,
        // !!! By necessity, this differs from the old compiler to better align with ParenthesizerRules. consider backporting
        Kind::PropertyAccessExpression | Kind::ElementAccessExpression | Kind::CallExpression => {
            if flags.intersects(OperatorPrecedenceFlags::OptionalChain) {
                return OperatorPrecedence::OptionalChain;
            }
            OperatorPrecedence::Member
        }
        // !!! By necessity, this differs from the old compiler to better align with ParenthesizerRules. consider backporting
        Kind::NewExpression => {
            if flags.intersects(OperatorPrecedenceFlags::NewWithoutArguments) {
                return OperatorPrecedence::LeftHandSide;
            }
            OperatorPrecedence::Member
        }
        // !!! By necessity, this differs from the old compiler to better align with ParenthesizerRules. consider backporting
        Kind::TaggedTemplateExpression | Kind::MetaProperty | Kind::ExpressionWithTypeArguments => {
            OperatorPrecedence::Member
        }
        Kind::AsExpression | Kind::SatisfiesExpression => OperatorPrecedence::Relational,
        Kind::ThisKeyword
        | Kind::SuperKeyword
        | Kind::ImportKeyword
        | Kind::Identifier
        | Kind::PrivateIdentifier
        | Kind::NullKeyword
        | Kind::TrueKeyword
        | Kind::FalseKeyword
        | Kind::NumericLiteral
        | Kind::BigIntLiteral
        | Kind::StringLiteral
        | Kind::ArrayLiteralExpression
        | Kind::ObjectLiteralExpression
        | Kind::FunctionExpression
        | Kind::ClassExpression
        | Kind::RegularExpressionLiteral
        | Kind::NoSubstitutionTemplateLiteral
        | Kind::TemplateExpression
        | Kind::OmittedExpression
        | Kind::JsxElement
        | Kind::JsxSelfClosingElement
        | Kind::JsxFragment
        | Kind::MissingDeclaration => OperatorPrecedence::Primary,
        // !!! By necessity, this differs from the old compiler to support emit. consider backporting
        Kind::ParenthesizedExpression => OperatorPrecedence::Parentheses,
        _ => OperatorPrecedence::Invalid,
    }
}

// Gets the precedence of a binary operator
pub fn get_binary_operator_precedence(operator_kind: Kind) -> OperatorPrecedence {
    match operator_kind {
        Kind::QuestionQuestionToken => OperatorPrecedence::COALESCE,
        Kind::BarBarToken => OperatorPrecedence::LogicalOR,
        Kind::AmpersandAmpersandToken => OperatorPrecedence::LogicalAND,
        Kind::BarToken => OperatorPrecedence::BitwiseOR,
        Kind::CaretToken => OperatorPrecedence::BitwiseXOR,
        Kind::AmpersandToken => OperatorPrecedence::BitwiseAND,
        Kind::EqualsEqualsToken
        | Kind::ExclamationEqualsToken
        | Kind::EqualsEqualsEqualsToken
        | Kind::ExclamationEqualsEqualsToken => OperatorPrecedence::Equality,
        Kind::LessThanToken
        | Kind::GreaterThanToken
        | Kind::LessThanEqualsToken
        | Kind::GreaterThanEqualsToken
        | Kind::InstanceOfKeyword
        | Kind::InKeyword
        | Kind::AsKeyword
        | Kind::SatisfiesKeyword => OperatorPrecedence::Relational,
        Kind::LessThanLessThanToken
        | Kind::GreaterThanGreaterThanToken
        | Kind::GreaterThanGreaterThanGreaterThanToken => OperatorPrecedence::Shift,
        Kind::PlusToken | Kind::MinusToken => OperatorPrecedence::Additive,
        Kind::AsteriskToken | Kind::SlashToken | Kind::PercentToken => {
            OperatorPrecedence::Multiplicative
        }
        Kind::AsteriskAsteriskToken => OperatorPrecedence::Exponentiation,
        // -1 is lower than all other precedences.  Returning it will cause binary expression
        // parsing to stop.
        _ => OperatorPrecedence::Invalid,
    }
}

// Gets the leftmost expression of an expression, e.g. `a` in `a.b`, `a[b]`, `a++`, `a+b`, `a?b:c`, `a as B`, etc.
pub fn get_leftmost_expression(mut node: P<Node>, stop_at_call_expressions: bool) -> P<Node> {
    loop {
        match node.kind() {
            Kind::PostfixUnaryExpression => {
                node = node.as_postfix_unary_expression().operand;
                continue;
            }
            Kind::BinaryExpression => {
                node = node.as_binary_expression().left;
                continue;
            }
            Kind::ConditionalExpression => {
                node = node.as_conditional_expression().condition;
                continue;
            }
            Kind::TaggedTemplateExpression => {
                node = node.as_tagged_template_expression().tag;
                continue;
            }
            Kind::CallExpression if stop_at_call_expressions => {
                return node;
            }
            Kind::CallExpression
            | Kind::AsExpression
            | Kind::ElementAccessExpression
            | Kind::PropertyAccessExpression
            | Kind::NonNullExpression
            | Kind::PartiallyEmittedExpression
            | Kind::SatisfiesExpression => {
                node = node.expression().unwrap();
                continue;
            }
            _ => {}
        }
        return node;
    }
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum TypePrecedence {
    // Conditional precedence (lowest)
    Conditional,
    // JSDoc precedence (optional and variadic types)
    JSDoc,
    // Function precedence
    Function,
    // Union precedence
    Union,
    // Intersection precedence
    Intersection,
    // TypeOperatorNode precedence
    TypeOperator,
    // Postfix precedence
    Postfix,
    // NonArray precedence (highest)
    NonArray,
}

impl TypePrecedence {
    pub const LOWEST: TypePrecedence = TypePrecedence::Conditional;
    pub const HIGHEST: TypePrecedence = TypePrecedence::NonArray;
}

// Gets the precedence of a TypeNode
pub fn get_type_node_precedence(n: P<Node>) -> TypePrecedence {
    match n.kind() {
        Kind::ConditionalType => TypePrecedence::Conditional,
        Kind::JSDocOptionalType | Kind::JSDocVariadicType => TypePrecedence::JSDoc,
        Kind::FunctionType | Kind::ConstructorType => TypePrecedence::Function,
        Kind::UnionType => TypePrecedence::Union,
        Kind::IntersectionType => TypePrecedence::Intersection,
        // TypeQueryNode is actually a NonArrayType, but we treat it as TypeOperatorNode
        // precedence so that it is parenthesized when used in a PostfixType
        // context (e.g., `(typeof C)[]` instead of `typeof C[]`)
        Kind::TypeOperator | Kind::TypeQuery => TypePrecedence::TypeOperator,
        Kind::InferType => {
            if n.as_infer_type_node().type_parameter.as_type_parameter_declaration().constraint.is_some() {
                // `infer T extends U` must be treated as FunctionTypeNode precedence as the `extends` clause eagerly consumes
                // TypeNode
                return TypePrecedence::Function;
            }
            TypePrecedence::TypeOperator
        }
        Kind::IndexedAccessType | Kind::ArrayType | Kind::OptionalType => TypePrecedence::Postfix,
        Kind::AnyKeyword
        | Kind::UnknownKeyword
        | Kind::StringKeyword
        | Kind::NumberKeyword
        | Kind::BigIntKeyword
        | Kind::SymbolKeyword
        | Kind::BooleanKeyword
        | Kind::UndefinedKeyword
        | Kind::NeverKeyword
        | Kind::ObjectKeyword
        | Kind::IntrinsicKeyword
        | Kind::VoidKeyword
        | Kind::JSDocAllType
        | Kind::JSDocNullableType
        | Kind::JSDocNonNullableType
        | Kind::LiteralType
        | Kind::TypePredicate
        | Kind::TypeReference
        | Kind::TypeLiteral
        | Kind::TupleType
        | Kind::RestType
        | Kind::ParenthesizedType
        | Kind::ThisType
        | Kind::MappedType
        | Kind::NamedTupleMember
        | Kind::TemplateLiteralType
        | Kind::ImportType
        // These occur in pseudo-types like `f<T>.C`, where `f` is a generic function and `C` is a local type
        | Kind::PropertyAccessExpression
        | Kind::ExpressionWithTypeArguments => TypePrecedence::NonArray,
        _ => panic!("unhandled TypeNode: {:?}", n.kind()),
    }
}
