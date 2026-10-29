use crate::*;
use tsrs_checker::LiteralValue;

struct ConstEnumInliningTransformer {
    base: Transformer,
    compiler_options: P<CompilerOptions>,
    emit_resolver: Option<Resolver>,
}

// constenum.go:22
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_const_enum_inlining_transformer(opt: &TransformOptions) -> Option<P<Transformer>> {
    let compiler_options = opt.compiler_options;
    let emit_context = opt.context;
    assert!(
        !compiler_options.get_isolated_modules(),
        "const enums are not inlined under isolated modules"
    );
    let tx = P::new(ConstEnumInliningTransformer {
        base: Transformer::default(),
        compiler_options,
        emit_resolver: opt.emit_resolver,
    });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(emit_context),
    ))
}

impl ConstEnumInliningTransformer {
    // constenum.go:32
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        let emit_context = self.base.emit_context();
        let f = self.base.factory();
        if let Kind::PropertyAccessExpression | Kind::ElementAccessExpression = node.kind() {
            let Some(parse) = emit_context.parse_node(Some(node)) else {
                return self.base.visitor().visit_each_child(Some(node));
            };
            let value = self.emit_resolver.unwrap().get_constant_value(parse);
            if let Some(value) = value {
                let mut replacement: Option<P<Node>> = None;
                match value {
                    LiteralValue::Number(v) => {
                        if v.is_inf() {
                            if v.abs() == v {
                                replacement = Some(f.new_identifier("Infinity"));
                            } else {
                                replacement = Some(f.new_prefix_unary_expression(
                                    Kind::MinusToken,
                                    f.new_identifier("Infinity"),
                                ));
                            }
                        } else if v.is_nan() {
                            replacement = Some(f.new_identifier("NaN"));
                        } else if v.abs() == v {
                            replacement = Some(
                                f.new_numeric_literal(alloc_str(&v.string()), TokenFlags::None),
                            );
                        } else {
                            replacement = Some(f.new_prefix_unary_expression(
                                Kind::MinusToken,
                                f.new_numeric_literal(
                                    alloc_str(&v.abs().string()),
                                    TokenFlags::None,
                                ),
                            ));
                        }
                    }
                    LiteralValue::String(v) => {
                        replacement = Some(f.new_string_literal(v, TokenFlags::None));
                    }
                    LiteralValue::BigInt(v) => {
                        // technically not supported by strada, and issues a checker error, handled here for completeness
                        if v == jsnum::PseudoBigInt::default() {
                            replacement = Some(f.new_big_int_literal("0", TokenFlags::None));
                        } else if !v.negative {
                            replacement =
                                Some(f.new_big_int_literal(v.base10_value, TokenFlags::None));
                        } else {
                            replacement = Some(f.new_prefix_unary_expression(
                                Kind::MinusToken,
                                f.new_big_int_literal(v.base10_value, TokenFlags::None),
                            ));
                        }
                    }
                    LiteralValue::Boolean(_) => {}
                }

                if self.compiler_options.remove_comments.is_false_or_unknown() {
                    let original = emit_context.most_original(Some(node));
                    if let Some(original) = original
                        && !ast::node_is_synthesized(original)
                    {
                        let original_text = scanner::get_text_of_node(original);
                        let escaped_text = safe_multi_line_comment(&original_text);
                        // Go dereferences a nil replacement here (a boolean constant value cannot occur).
                        emit_context.add_synthetic_trailing_comment(
                            replacement.unwrap(),
                            Kind::MultiLineCommentTrivia,
                            &escaped_text,
                            false,
                        );
                    }
                }
                return replacement;
            }
            return self.base.visitor().visit_each_child(Some(node));
        }
        self.base.visitor().visit_each_child(Some(node))
    }
}

// constenum.go:86
fn safe_multi_line_comment(text: &str) -> String {
    let mut b = String::with_capacity(text.len() + 2);
    b.push(' ');
    b.extend(
        text.split("*/").enumerate().flat_map(|(i, part)| [if i == 0 { "" } else { "*_/" }, part]),
    );
    b.push(' ');
    b
}
