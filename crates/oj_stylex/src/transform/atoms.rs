//! `@stylexjs/atoms` recognition: the compile-away sugar the babel plugin
//! bundles from `@stylexjs/atoms/babel-transform` and runs in `Program.exit`.

use oxc_ast::ast::{CallExpression, Expression, IdentifierReference};
use oxc_span::{GetSpan, Span};
use oxc_syntax::node::NodeId;

use crate::imports::ATOM_NAMESPACE;
use crate::state::CompileState;

/// A recognised atom expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtomStyle {
    /// `x.display.flex` / `color.red` → one raw declaration.
    Static { property: String, value: String },
    /// `x.color(v)` → a hoisted arrow taking the call's single argument.
    Dynamic { property: String },
}

/// A member expression split the way the atoms transform reads it.
pub struct MemberParts<'a> {
    pub object: &'a Expression<'a>,
    /// `getPropKey(node.property, node.computed)`.
    pub key: Option<String>,
}

// parity: atoms babel-transform getPropKey. Anything else (template literals,
// identifiers used computed) reads as "no key", never as an error.
pub fn member_parts<'a>(expr: &'a Expression<'a>) -> Option<MemberParts<'a>> {
    match expr {
        Expression::StaticMemberExpression(m) if !m.optional => Some(MemberParts {
            object: &m.object,
            key: Some(m.property.name.to_string()),
        }),
        Expression::ComputedMemberExpression(m) if !m.optional => Some(MemberParts {
            object: &m.object,
            key: match &m.expression {
                Expression::StringLiteral(s) => Some(s.value.to_string()),
                Expression::NumericLiteral(n) => Some(crate::jsrt::js_number_to_string(n.value)),
                _ => None,
            },
        }),
        _ => None,
    }
}

/// parity: atoms babel-transform normalizeValue — one leading `_` only.
pub fn normalize_value(value: &str) -> &str {
    value.strip_prefix('_').unwrap_or(value)
}

/// parity: isUtilityStylesIdentifier. The `atomImports` hit is scope-blind
/// upstream; the binding fallback below is not, and reaches `import type` too.
pub fn is_atom_identifier(
    base: &IdentifierReference<'_>,
    node_id: NodeId,
    state: &CompileState<'_>,
) -> bool {
    state.imports.atom_import(&base.name).is_some()
        || (state.imports.is_atom_binding_local(&base.name)
            && state.resolves_to_root_binding(node_id, &base.name))
}

/// parity: getStaticStyleFromPath. `object`/`key` come from the member the
/// caller is visiting.
pub fn static_style(
    object: &Expression<'_>,
    key: Option<&str>,
    node_id: NodeId,
    state: &CompileState<'_>,
) -> Option<AtomStyle> {
    let value_key = key?;
    if let Some(inner) = member_parts(object)
        && let Some(property) = inner.key
        && let Expression::Identifier(base) = inner.object
        && is_atom_identifier(base, node_id, state)
    {
        return Some(AtomStyle::Static {
            property,
            value: normalize_value(value_key).to_string(),
        });
    }
    let Expression::Identifier(base) = object else {
        return None;
    };
    if !is_atom_identifier(base, node_id, state) {
        return None;
    }
    // The single-level form reads atomImports directly, so a binding reachable
    // only through the scope fallback (a type import) stays inert here.
    let imported = state.imports.atom_import(&base.name)?;
    let property = if imported == ATOM_NAMESPACE {
        value_key.to_string()
    } else {
        imported.to_string()
    };
    Some(AtomStyle::Static {
        property,
        value: normalize_value(value_key).to_string(),
    })
}

/// parity: getDynamicStyleFromPath. Returns the property plus the single
/// argument's span; zero or two arguments leave the call untouched.
pub fn dynamic_style<'a>(
    call: &'a CallExpression<'a>,
    node_id: NodeId,
    state: &CompileState<'_>,
) -> Option<(AtomStyle, Span)> {
    let callee = member_parts(&call.callee)?;
    let value_key = callee.key?;
    if call.arguments.len() != 1 {
        return None;
    }
    let arg = call.arguments[0].as_expression()?;
    let property = match callee.object {
        Expression::Identifier(base) if is_atom_identifier(base, node_id, state) => value_key,
        object => {
            let inner = member_parts(object)?;
            let property = inner.key?;
            let Expression::Identifier(base) = inner.object else {
                return None;
            };
            if !is_atom_identifier(base, node_id, state) {
                return None;
            }
            property
        }
    };
    Some((AtomStyle::Dynamic { property }, arg.span()))
}
