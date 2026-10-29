use super::*;

/// Go `constantExpression(value any, ...)`: `value` is a string or a `jsnum.Number`; anything else yields nil.
#[derive(Clone, Copy)]
pub enum ConstantValue<'a> {
    String(&'a str),
    Number(Number),
}

// utilities.go:9
pub fn constant_expression(value: ConstantValue, factory: &printer::NodeFactory) -> P<Node> {
    match value {
        ConstantValue::String(value) => {
            factory.new_string_literal(alloc_str(value), TokenFlags::None)
        }
        ConstantValue::Number(value) => {
            if value.is_inf() {
                if value.0 > 0.0 {
                    return factory.new_identifier("Infinity");
                }
                return factory.new_prefix_unary_expression(
                    Kind::MinusToken,
                    factory.new_identifier("Infinity"),
                );
            }
            if value.is_nan() {
                return factory.new_identifier("NaN");
            }
            if value.0 < 0.0 {
                return factory.new_prefix_unary_expression(
                    Kind::MinusToken,
                    constant_expression(ConstantValue::Number(-value), factory),
                );
            }
            factory.new_numeric_literal(alloc_str(&value.string()), TokenFlags::None)
        }
    }
}
