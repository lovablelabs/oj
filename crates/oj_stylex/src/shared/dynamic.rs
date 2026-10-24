//! Function-valued namespaces -> CSS-variable rules + a structured arrow value.
// parity: visitors/parse-stylex-create-arg.js + stylex-create.js:228-485

use oxc_ast::ast::Expression;
use oxc_span::{GetSpan, Span};

use crate::eval::unwrap_parens;
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::hash::hash;
use crate::options::{ResolvedOptions, StyleResolution};
use crate::rules::StylexRule;
use crate::shared::flatten::StyleScalar;
use crate::shared::resolution::flat_map_expanded_shorthands;
use crate::shared::transform_value::get_number_suffix;

/// One CSS-variable-backed dynamic leaf, recorded during create-arg evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineStyle {
    pub key_path: Vec<String>,
    /// Span of the original (paren-unwrapped) leaf expression in the source.
    pub span: Span,
    /// "" ⇒ plain `expr != null ? expr : undefined`; else the
    /// `((val) => typeof val === "number" ? val + "<unit>" : …)(expr)` coercion.
    pub unit: String,
    pub has_nullish_fallback: bool,
    pub safe_to_skip_null_check: bool,
}

/// The `fns[namespace]` payload of parse-stylex-create-arg.js.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DynamicFn {
    pub params: Vec<String>,
    /// varName → leaf; insertion-ordered, re-writes keep the first position.
    pub inline_styles: Vec<(String, InlineStyle)>,
}

impl DynamicFn {
    pub fn insert_inline(&mut self, var_name: String, style: InlineStyle) {
        match self.inline_styles.iter_mut().find(|(n, _)| *n == var_name) {
            Some(slot) => slot.1 = style,
            None => self.inline_styles.push((var_name, style)),
        }
    }
}

/// Builds the var name + metadata for one non-static leaf.
// parity: parse-stylex-create-arg.js:181-248
pub fn inline_style_for_leaf(
    expr: &Expression<'_>,
    key_path: &[String],
    key: &str,
) -> (String, InlineStyle) {
    let expr = unwrap_parens(expr);
    let mut full_key_path: Vec<String> = key_path.to_vec();
    full_key_path.push(key.to_string());
    let var_name = if key_path.is_empty() {
        format!("--x-{key}")
    } else {
        format!("--x-{}", hash(&full_key_path.join("_")))
    };
    let prop_name = full_key_path
        .iter()
        .find(|k| !k.starts_with(':') && !k.starts_with('@') && *k != "default")
        .map(String::as_str)
        .unwrap_or(key);
    let unit = if is_time_unit_prop(prop_name) || is_length_unit_prop(prop_name) {
        get_number_suffix(prop_name)
    } else {
        ""
    };
    let style = InlineStyle {
        key_path: full_key_path,
        span: expr.span(),
        unit: unit.to_string(),
        has_nullish_fallback: has_explicit_nullish_fallback(expr),
        safe_to_skip_null_check: is_safe_to_skip_null_check(expr),
    };
    (var_name, style)
}

// parity: stylex-create.js isSafeToSkipNullCheck (babel ASTs carry no parens).
fn is_safe_to_skip_null_check(expr: &Expression<'_>) -> bool {
    match unwrap_parens(expr) {
        Expression::TemplateLiteral(_)
        | Expression::StringLiteral(_)
        | Expression::NumericLiteral(_)
        | Expression::BooleanLiteral(_) => true,
        Expression::BinaryExpression(bin) => {
            matches!(bin.operator.as_str(), "+" | "-" | "*" | "/" | "%" | "**")
        }
        Expression::UnaryExpression(unary) => matches!(unary.operator.as_str(), "-" | "+"),
        Expression::ConditionalExpression(cond) => {
            is_safe_to_skip_null_check(&cond.consequent)
                && is_safe_to_skip_null_check(&cond.alternate)
        }
        Expression::LogicalExpression(logical) => match logical.operator.as_str() {
            "??" | "||" => {
                is_safe_to_skip_null_check(&logical.left)
                    || is_safe_to_skip_null_check(&logical.right)
            }
            "&&" => {
                is_safe_to_skip_null_check(&logical.left)
                    && is_safe_to_skip_null_check(&logical.right)
            }
            _ => false,
        },
        _ => false,
    }
}

// parity: stylex-create.js hasExplicitNullishFallback
fn has_explicit_nullish_fallback(expr: &Expression<'_>) -> bool {
    match unwrap_parens(expr) {
        Expression::NullLiteral(_) => true,
        Expression::Identifier(id) => id.name == "undefined",
        Expression::UnaryExpression(unary) => unary.operator.as_str() == "void",
        Expression::ConditionalExpression(cond) => {
            has_explicit_nullish_fallback(&cond.consequent)
                || has_explicit_nullish_fallback(&cond.alternate)
        }
        Expression::LogicalExpression(logical) => {
            has_explicit_nullish_fallback(&logical.left)
                || has_explicit_nullish_fallback(&logical.right)
        }
        _ => false,
    }
}

/// One piece of a rewritten class-string concatenation.
#[derive(Debug, Clone, PartialEq)]
pub enum ClassPart {
    /// String-literal chunk (class name plus trailing space when not last).
    Lit(String),
    /// `<expr> != null ? "<lit>" : <expr>` — expr printed from its span.
    Guarded { lit: String, expr: Span },
}

#[derive(Debug, Clone, PartialEq)]
pub struct InlineVar {
    pub var_name: String,
    /// "" ⇒ nullish wrapper, else the typeof-number unit-coercion wrapper.
    pub unit: String,
    pub expr: Span,
}

/// Compiled arrow: `(params) => [staticObj?, conditionalObj?, vars]` (bare vars
/// when both partitions empty); `$$css` keys carry `css_tag`.
#[derive(Debug, Clone, PartialEq)]
pub struct DynamicCompiled {
    pub params: Vec<String>,
    /// key → string-literal segments; empty vec prints/evaluates as "".
    pub static_props: Vec<(String, Vec<String>)>,
    pub conditional_props: Vec<(String, Vec<ClassPart>)>,
    pub css_tag: EvalValue,
    pub inline_vars: Vec<InlineVar>,
}

impl DynamicCompiled {
    /// False only for namespaces with no compiled props: the arrow returns the
    /// bare vars object and the `$$css` tag is dropped entirely (upstream).
    pub fn has_container(&self) -> bool {
        !self.static_props.is_empty() || !self.conditional_props.is_empty()
    }
}

/// Compiled namespace -> arrow value; `injected_lookup` mirrors the visitor's
/// merged injectedStyles map. parity: stylex-create.js:294-485 fns branch.
pub fn compile_dynamic_namespace(
    compiled_ns: &JsObjectMap,
    class_paths: &[(String, Vec<String>)],
    fn_def: &DynamicFn,
    injected_lookup: &[StylexRule],
    options: &ResolvedOptions,
) -> DynamicCompiled {
    let orig_class_paths: Vec<(&str, String)> = class_paths
        .iter()
        .map(|(class_name, path)| (class_name.as_str(), path.join("_")))
        .collect();
    let dynamic_styles: Vec<(String, &InlineStyle)> =
        if options.style_resolution == StyleResolution::LegacyExpandShorthands {
            legacy_expand_dynamic_styles(fn_def, options)
        } else {
            fn_def
                .inline_styles
                .iter()
                .map(|(_, style)| (style.key_path.join("_"), style))
                .collect()
        };
    let nullish_vars: Vec<(&str, &InlineStyle)> = fn_def
        .inline_styles
        .iter()
        .filter(|(_, style)| style.has_nullish_fallback)
        .map(|(name, style)| (name.as_str(), style))
        .collect();

    let mut css_tag = EvalValue::Bool(true);
    let mut static_props: Vec<(String, Vec<String>)> = Vec::new();
    let mut conditional_props: Vec<(String, Vec<ClassPart>)> = Vec::new();

    for (key, value) in compiled_ns.entries() {
        if key == "$$css" {
            css_tag = value.clone();
            continue;
        }
        // JS "".split(' ') is [""]; both roads end at a static "" prop.
        let class_list: Vec<&str> = match value {
            EvalValue::Str(s) => s.split(' ').collect(),
            _ => Vec::new(),
        };
        let mut is_static = true;
        let mut parts: Vec<ClassPart> = Vec::new();
        for (index, cls) in class_list.iter().enumerate() {
            let mut expr: Option<&InlineStyle> = orig_class_paths
                .iter()
                .find(|(name, _)| name == cls)
                .and_then(|(_, path)| {
                    dynamic_styles
                        .iter()
                        .find(|(p, _)| p == path)
                        .map(|(_, style)| *style)
                });
            if expr.is_none() && !nullish_vars.is_empty() {
                expr = injected_lookup
                    .iter()
                    .find(|rule| &*rule.class_name == *cls)
                    .and_then(|rule| {
                        placeholder_var_names(&rule.ltr).into_iter().find_map(|v| {
                            nullish_vars
                                .iter()
                                .find(|(name, _)| *name == v)
                                .map(|(_, style)| *style)
                        })
                    });
            }
            let is_last = index == class_list.len() - 1;
            let lit = if is_last {
                (*cls).to_string()
            } else {
                format!("{cls} ")
            };
            match expr {
                Some(style) if !style.safe_to_skip_null_check => {
                    is_static = false;
                    parts.push(ClassPart::Guarded {
                        lit,
                        expr: style.span,
                    });
                }
                _ => parts.push(ClassPart::Lit(lit)),
            }
        }
        if is_static {
            let segments = parts
                .into_iter()
                .map(|part| match part {
                    ClassPart::Lit(lit) => lit,
                    ClassPart::Guarded { .. } => unreachable!("static props hold literals only"),
                })
                .collect();
            static_props.push((key.to_string(), segments));
        } else {
            conditional_props.push((key.to_string(), parts));
        }
    }

    DynamicCompiled {
        params: fn_def.params.clone(),
        static_props,
        conditional_props,
        css_tag,
        inline_vars: fn_def
            .inline_styles
            .iter()
            .map(|(var_name, style)| InlineVar {
                var_name: var_name.clone(),
                unit: style.unit.clone(),
                expr: style.span,
            })
            .collect(),
    }
}

/// The path each leaf is looked up by: everything up to and including the
/// first segment that is not a pseudo/at-rule condition.
fn truncated_key(key_path: &[String]) -> String {
    let end = key_path
        .iter()
        .position(|k| !k.starts_with(':') && !k.starts_with('@'))
        .map_or(key_path.len(), |i| i + 1);
    key_path[..end].join("_")
}

/// One entry per expanded longhand, key and path rewritten; nulls drop out.
// parity: visitors/stylex-create.js:333 legacyExpandShorthands
fn legacy_expand_dynamic_styles<'a>(
    fn_def: &'a DynamicFn,
    options: &ResolvedOptions,
) -> Vec<(String, &'a InlineStyle)> {
    let mut out = Vec::with_capacity(fn_def.inline_styles.len());
    for (index, (_, style)) in fn_def.inline_styles.iter().enumerate() {
        let key = truncated_key(&style.key_path);
        let path = style.key_path.join("_");
        let placeholder = StyleScalar::Str(std::borrow::Cow::Owned(format!("p{index}")));
        let Ok(pairs) = flat_map_expanded_shorthands(
            std::borrow::Cow::Borrowed(key.as_str()),
            Some(placeholder),
            false,
            options,
        ) else {
            continue;
        };
        for (new_key, value) in pairs {
            if !matches!(value, Some(StyleScalar::Str(_))) {
                continue;
            }
            let new_path = if path == key {
                new_key.into_owned()
            } else if let Some(rest) = path.strip_prefix(&format!("{key}_")) {
                format!("{new_key}_{rest}")
            } else {
                path.replacen(&format!("_{key}"), &format!("_{new_key}"), 1)
            };
            out.push((new_path, style));
        }
    }
    out
}

/// The `/var\((--x-[^,)]+)[^)]*\)/g` capture list over one rule body.
fn placeholder_var_names(rule: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut search = 0;
    while let Some(pos) = rule[search..].find("var(") {
        let after = search + pos + 4;
        let rest = &rule[after..];
        if rest.starts_with("--x-") {
            let name_len = rest.find([',', ')']).unwrap_or(rest.len());
            if name_len > 4
                && let Some(close) = rest.find(')')
                && close >= name_len
            {
                names.push(&rest[..name_len]);
                search = after + close + 1;
                continue;
            }
        }
        search = after;
    }
    names
}

/// Per-variable `@property` rules (priority 0), deduped first-seen/last-wins.
// parity: stylex-create.js:228-248 injectedInheritStyles
pub fn inherit_rules(fns: &[(String, DynamicFn)]) -> Vec<StylexRule> {
    let mut rules: Vec<StylexRule> = Vec::new();
    for (_, fn_def) in fns {
        for (var_name, style) in &fn_def.inline_styles {
            // Pseudo-elements can only access css vars via inheritance.
            let inherits = style.key_path.iter().any(|k| k.starts_with("::"));
            let rule = StylexRule {
                class_name: var_name.as_str().into(),
                ltr: format!("@property {var_name} {{ syntax: \"*\"; inherits: {inherits};}}")
                    .into(),
                rtl: None,
                const_key: None,
                const_val: None,
                priority: 0.0,
            };
            match rules.iter_mut().find(|r| &*r.class_name == var_name) {
                Some(slot) => *slot = rule,
                None => rules.push(rule),
            }
        }
    }
    rules
}

fn is_time_unit_prop(key: &str) -> bool {
    matches!(
        key,
        "animationDelay"
            | "animationDuration"
            | "transitionDelay"
            | "transitionDuration"
            | "voiceDuration"
    )
}

// parity: transform-value.js lengthUnits (verbatim, duplicates and all)
fn is_length_unit_prop(key: &str) -> bool {
    matches!(
        key,
        "backgroundPositionX"
            | "backgroundPositionY"
            | "blockSize"
            | "borderBlockEndWidth"
            | "borderBlockStartWidth"
            | "borderBlockWidth"
            | "borderVerticalWidth"
            | "borderBottomLeftRadius"
            | "borderBottomRightRadius"
            | "borderBottomWidth"
            | "borderEndEndRadius"
            | "borderEndStartRadius"
            | "borderInlineEndWidth"
            | "borderEndWidth"
            | "borderInlineStartWidth"
            | "borderStartWidth"
            | "borderInlineWidth"
            | "borderHorizontalWidth"
            | "borderLeftWidth"
            | "borderRightWidth"
            | "borderSpacing"
            | "borderStartEndRadius"
            | "borderStartStartRadius"
            | "borderTopLeftRadius"
            | "borderTopRightRadius"
            | "borderTopWidth"
            | "bottom"
            | "columnGap"
            | "columnRuleWidth"
            | "columnWidth"
            | "containIntrinsicBlockSize"
            | "containIntrinsicHeight"
            | "containIntrinsicInlineSize"
            | "containIntrinsicWidth"
            | "flexBasis"
            | "fontSize"
            | "fontSmooth"
            | "height"
            | "inlineSize"
            | "insetBlockEnd"
            | "insetBlockStart"
            | "insetInlineEnd"
            | "insetInlineStart"
            | "left"
            | "letterSpacing"
            | "marginBlockEnd"
            | "marginBlockStart"
            | "marginBottom"
            | "marginInlineEnd"
            | "marginEnd"
            | "marginInlineStart"
            | "marginStart"
            | "marginLeft"
            | "marginRight"
            | "marginTop"
            | "maxBlockSize"
            | "maxHeight"
            | "maxInlineSize"
            | "maxWidth"
            | "minBlockSize"
            | "minHeight"
            | "minInlineSize"
            | "minWidth"
            | "offsetDistance"
            | "outlineOffset"
            | "outlineWidth"
            | "overflowClipMargin"
            | "paddingBlockEnd"
            | "paddingBlockStart"
            | "paddingBottom"
            | "paddingInlineEnd"
            | "paddingEnd"
            | "paddingInlineStart"
            | "paddingStart"
            | "paddingLeft"
            | "paddingRight"
            | "paddingTop"
            | "perspective"
            | "right"
            | "rowGap"
            | "scrollMarginBlockEnd"
            | "scrollMarginBlockStart"
            | "scrollMarginBottom"
            | "scrollMarginInlineEnd"
            | "scrollMarginInlineStart"
            | "scrollMarginLeft"
            | "scrollMarginRight"
            | "scrollMarginTop"
            | "scrollPaddingBlockEnd"
            | "scrollPaddingBlockStart"
            | "scrollPaddingBottom"
            | "scrollPaddingInlineEnd"
            | "scrollPaddingInlineStart"
            | "scrollPaddingLeft"
            | "scrollPaddingRight"
            | "scrollPaddingTop"
            | "scrollSnapMarginBottom"
            | "scrollSnapMarginLeft"
            | "scrollSnapMarginRight"
            | "scrollSnapMarginTop"
            | "shapeMargin"
            | "tabSize"
            | "textDecorationThickness"
            | "textIndent"
            | "textUnderlineOffset"
            | "top"
            | "transformOrigin"
            | "translate"
            | "verticalAlign"
            | "width"
            | "wordSpacing"
            | "border"
            | "borderBlock"
            | "borderBlockEnd"
            | "borderBlockStart"
            | "borderBottom"
            | "borderLeft"
            | "borderRadius"
            | "borderRight"
            | "borderTop"
            | "borderWidth"
            | "columnRule"
            | "containIntrinsicSize"
            | "gap"
            | "inset"
            | "insetBlock"
            | "insetInline"
            | "margin"
            | "marginBlock"
            | "marginVertical"
            | "marginInline"
            | "marginHorizontal"
            | "offset"
            | "outline"
            | "padding"
            | "paddingBlock"
            | "paddingVertical"
            | "paddingInline"
            | "paddingHorizontal"
            | "scrollMargin"
            | "scrollMarginBlock"
            | "scrollMarginInline"
            | "scrollPadding"
            | "scrollPaddingBlock"
            | "scrollPaddingInline"
            | "scrollSnapMargin"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_unit_pins() {
        // width: length prop → px; tabSize: length ∩ unitless → "" wrapper;
        // opacity/color: not in either set → nullish wrapper.
        let unit = |prop: &str| {
            if is_time_unit_prop(prop) || is_length_unit_prop(prop) {
                get_number_suffix(prop)
            } else {
                ""
            }
        };
        assert_eq!(unit("width"), "px");
        assert_eq!(unit("animationDuration"), "ms");
        assert_eq!(unit("tabSize"), "");
        assert_eq!(unit("opacity"), "");
        assert_eq!(unit("color"), "");
        assert_eq!(unit("strokeWidth"), "");
    }

    #[test]
    fn placeholder_var_name_scan_matches_regex() {
        assert_eq!(
            placeholder_var_names(".x{width:var(--x-width)}"),
            vec!["--x-width"]
        );
        assert_eq!(
            placeholder_var_names(".x{width:var(--x-a, 10px)}"),
            vec!["--x-a"]
        );
        assert_eq!(
            placeholder_var_names(".x{margin:var(--x-a) var(--x-b)}"),
            vec!["--x-a", "--x-b"]
        );
        // Non-placeholder vars and the bare prefix do not match.
        assert!(placeholder_var_names(".x{color:var(--other)}").is_empty());
        assert!(placeholder_var_names(".x{color:var(--x-)}").is_empty());
    }

    #[test]
    fn inherit_rules_dedupe_and_pseudo_element_inherit() {
        let style = |path: &[&str]| InlineStyle {
            key_path: path.iter().map(|s| s.to_string()).collect(),
            span: Span::default(),
            unit: String::new(),
            has_nullish_fallback: false,
            safe_to_skip_null_check: false,
        };
        let fns = vec![
            (
                "a".to_string(),
                DynamicFn {
                    params: vec!["w".to_string()],
                    inline_styles: vec![("--x-width".to_string(), style(&["width"]))],
                },
            ),
            (
                "b".to_string(),
                DynamicFn {
                    params: vec!["w".to_string()],
                    inline_styles: vec![
                        ("--x-width".to_string(), style(&["width"])),
                        ("--x-abc".to_string(), style(&["::before", "width"])),
                    ],
                },
            ),
        ];
        let rules = inherit_rules(&fns);
        assert_eq!(rules.len(), 2);
        assert_eq!(
            &*rules[0].ltr,
            "@property --x-width { syntax: \"*\"; inherits: false;}"
        );
        assert_eq!(
            &*rules[1].ltr,
            "@property --x-abc { syntax: \"*\"; inherits: true;}"
        );
        assert!(rules.iter().all(|r| r.priority == 0.0));
    }
}
