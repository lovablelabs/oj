//! Injected-callable seam between the evaluator and the emit-API modules.
// parity: stylex-create.js:140-206 FunctionConfig wiring

use std::sync::Arc;

use crate::errors::StylexError;
use crate::eval::value::EvalValue;
use crate::state::CompileState;

/// The compile-time callables `stylex.create` evaluation may dispatch to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StylexCallable {
    FirstThatWorks,
    Keyframes,
    PositionTry,
    DefaultMarker,
    WhenAncestor,
    WhenDescendant,
    WhenSiblingBefore,
    WhenSiblingAfter,
    WhenAnySibling,
    /// `types.<name>(…)` for defineVars contexts (integrator-wired).
    Types(String),
    /// `unstable_conditional(x)` — the upstream identity fn for typing.
    Conditional,
}

pub const WHEN_MEMBERS: [(&str, StylexCallable); 5] = [
    ("ancestor", StylexCallable::WhenAncestor),
    ("descendant", StylexCallable::WhenDescendant),
    ("siblingBefore", StylexCallable::WhenSiblingBefore),
    ("siblingAfter", StylexCallable::WhenSiblingAfter),
    ("anySibling", StylexCallable::WhenAnySibling),
];

/// The seam the emit-API slice plugs its real semantics into; `state` lets
/// keyframes/positionTry record their injectable rules.
pub trait EvalCallables {
    fn call(
        &self,
        callee: &StylexCallable,
        args: &[EvalValue],
        state: &mut CompileState<'_>,
    ) -> Result<EvalValue, StylexError>;
}

/// Placeholder while the emit-API modules are mid-flight: every dispatch is a
/// structured unsupported_api error naming the callable.
pub struct StubCallables;

impl EvalCallables for StubCallables {
    fn call(
        &self,
        callee: &StylexCallable,
        _args: &[EvalValue],
        _state: &mut CompileState<'_>,
    ) -> Result<EvalValue, StylexError> {
        let name = match callee {
            StylexCallable::FirstThatWorks => "firstThatWorks",
            StylexCallable::Keyframes => "keyframes",
            StylexCallable::PositionTry => "positionTry",
            StylexCallable::DefaultMarker => "defaultMarker",
            StylexCallable::WhenAncestor => "when.ancestor",
            StylexCallable::WhenDescendant => "when.descendant",
            StylexCallable::WhenSiblingBefore => "when.siblingBefore",
            StylexCallable::WhenSiblingAfter => "when.siblingAfter",
            StylexCallable::WhenAnySibling => "when.anySibling",
            StylexCallable::Types(_) => "types.*",
            StylexCallable::Conditional => "unstable_conditional",
        };
        Err(StylexError::unsupported_api(name))
    }
}

// ---- theming (defineVars / createTheme) evaluator support ------------------

use oxc_ast::ast::{
    Argument, ArrayExpressionElement, CallExpression, Expression, ObjectPropertyKind, PropertyKind,
    UnaryOperator,
};
use oxc_span::GetSpan;

use crate::eval::cross_file::VarGroupProxy;
use crate::eval::{
    Callable, EvalOutcome, Evaluator, FunctionRegistry, JsObj, JsValue, RegistryEntry,
    collect_member_chain, from_eval_value, is_invalid_method, is_valid_callee, js_to_string,
    truthy, unwrap_parens,
};
use crate::imports::{ImportTable, StylexNamedImport};
use crate::shared::types::TYPES_MEMBERS;

fn types_object() -> JsValue {
    let mut obj = JsObj::default();
    for name in TYPES_MEMBERS {
        obj.insert(
            name.to_string(),
            JsValue::Callable(Callable::Stylex(StylexCallable::Types(name.to_string()))),
        );
    }
    JsValue::object(obj)
}

/// FunctionConfig of visitors/stylex-define-vars.js and the second argument of
/// visitors/stylex-create-theme.js: keyframes, positionTry, types, env.
pub fn vars_registry(imports: &ImportTable, env: &EvalValue) -> FunctionRegistry {
    let mut registry = FunctionRegistry::default();
    for (local, binding) in &imports.named {
        let entry = match binding {
            StylexNamedImport::Keyframes => RegistryEntry::Callable(StylexCallable::Keyframes),
            StylexNamedImport::PositionTry => RegistryEntry::Callable(StylexCallable::PositionTry),
            StylexNamedImport::Types => RegistryEntry::Value(types_object()),
            StylexNamedImport::Env => RegistryEntry::Value(from_eval_value(env)),
            StylexNamedImport::Conditional => RegistryEntry::Callable(StylexCallable::Conditional),
            _ => continue,
        };
        registry.identifiers.insert(local.clone(), entry);
    }
    for namespace in &imports.stylex_namespaces {
        let mut obj = JsObj::default();
        obj.insert("types".to_string(), types_object());
        obj.insert("env".to_string(), from_eval_value(env));
        registry.identifiers.insert(
            namespace.clone(),
            RegistryEntry::Value(JsValue::object(obj)),
        );
        for (name, callable) in [
            ("keyframes", StylexCallable::Keyframes),
            ("positionTry", StylexCallable::PositionTry),
            ("unstable_conditional", StylexCallable::Conditional),
        ] {
            registry.add_member_callable(namespace, name, callable);
        }
    }
    registry
}

/// Mirrors `identifiers[varId.name] = selfReferenceProxy` (set after the first
/// evaluation, visible to function leaves invoked during normalization).
pub fn register_self_reference(
    ev: &mut Evaluator<'_, '_>,
    export_name: &str,
    proxy: VarGroupProxy,
) {
    ev.registry.to_mut().identifiers.insert(
        export_name.to_string(),
        RegistryEntry::Value(JsValue::proxy(proxy)),
    );
}

/// Body and param count of an evaluated expression-body arrow (`Callable::Arrow`).
pub fn arrow_info<'a>(ev: &Evaluator<'a, '_>, key: u32) -> Option<(&'a Expression<'a>, usize)> {
    ev.arrow_bodies
        .get(&key)
        .map(|(body, params)| (*body, params.len()))
}

/// The self-reference proxy's `onAccess` stand-in: walks a function body in
/// evaluation order (lazy conditionals, last-only sequences, eager logicals).
pub struct DepTracker<'p> {
    self_proxy: &'p VarGroupProxy,
    pub deps: Vec<String>,
    depth: u32,
}

impl<'p> DepTracker<'p> {
    pub fn new(self_proxy: &'p VarGroupProxy) -> Self {
        DepTracker {
            self_proxy,
            deps: Vec::new(),
            depth: 0,
        }
    }

    fn record(&mut self, key: String) {
        if !self.deps.iter().any(|d| d == &key) {
            self.deps.push(key);
        }
    }

    // The proxy's __IS_PROXY/toString/__varGroupHash__ traps bypass onAccess.
    fn record_access(&mut self, key: &str) {
        if !matches!(key, "__IS_PROXY" | "__varGroupHash__" | "toString") {
            self.record(key.to_string());
        }
    }

    fn is_self(&self, value: &JsValue) -> bool {
        matches!(value, JsValue::Proxy(p) if p.as_ref() == self.self_proxy)
    }

    fn eval_value<'a>(
        &mut self,
        ev: &mut Evaluator<'a, '_>,
        expr: &'a Expression<'a>,
    ) -> Option<JsValue> {
        match ev.eval(expr) {
            Ok(EvalOutcome::Value(v)) => Some(v),
            _ => None,
        }
    }

    /// `None` halts the walk: the real evaluation deopts or errors right there.
    pub fn walk<'a>(&mut self, ev: &mut Evaluator<'a, '_>, expr: &'a Expression<'a>) -> Option<()> {
        self.depth += 1;
        let result = if self.depth > 128 {
            None
        } else {
            self.walk_inner(ev, expr)
        };
        self.depth -= 1;
        result
    }

    fn walk_inner<'a>(
        &mut self,
        ev: &mut Evaluator<'a, '_>,
        expr: &'a Expression<'a>,
    ) -> Option<()> {
        match expr {
            Expression::ParenthesizedExpression(e) => self.walk(ev, &e.expression),
            Expression::TSAsExpression(e) => self.walk(ev, &e.expression),
            Expression::TSSatisfiesExpression(e) => self.walk(ev, &e.expression),
            Expression::ArrowFunctionExpression(_) => Some(()),
            Expression::SequenceExpression(seq) => match seq.expressions.last() {
                Some(last) => self.walk(ev, last),
                None => None,
            },
            Expression::StringLiteral(_)
            | Expression::NumericLiteral(_)
            | Expression::BooleanLiteral(_)
            | Expression::NullLiteral(_) => Some(()),
            Expression::TemplateLiteral(tpl) => {
                for expr in &tpl.expressions {
                    self.walk(ev, expr)?;
                }
                Some(())
            }
            Expression::ConditionalExpression(cond) => {
                self.walk(ev, &cond.test)?;
                let test = self.eval_value(ev, &cond.test)?;
                if truthy(&test) {
                    self.walk(ev, &cond.consequent)
                } else {
                    self.walk(ev, &cond.alternate)
                }
            }
            Expression::StaticMemberExpression(_) | Expression::ComputedMemberExpression(_) => {
                self.walk_member(ev, expr)
            }
            Expression::Identifier(id) => {
                let name = id.name.as_str();
                if ev.registry.identifiers.contains_key(name) {
                    return Some(());
                }
                let Some(symbol) = ev.state.symbol_of(id) else {
                    return match name {
                        "undefined" | "Infinity" | "NaN" => Some(()),
                        _ => None,
                    };
                };
                let info = ev.state.binding_info(symbol);
                if matches!(
                    info.decl,
                    crate::state::BindingDecl::NamedImport
                        | crate::state::BindingDecl::DefaultImport
                        | crate::state::BindingDecl::NamespaceImport
                ) {
                    return Some(());
                }
                if ev.state.is_non_constant(symbol)
                    || ev.state.is_mutated(symbol)
                    || id.span.start < info.span.end
                {
                    return None;
                }
                if ev.state.binding_override(symbol).is_some() {
                    return Some(());
                }
                match info.decl {
                    crate::state::BindingDecl::Declarator(declarator)
                        if declarator.id.get_binding_identifier().is_some() =>
                    {
                        match &declarator.init {
                            Some(init) => self.walk(ev, init),
                            None => None,
                        }
                    }
                    _ => None,
                }
            }
            Expression::UnaryExpression(unary) => match unary.operator {
                UnaryOperator::Void => Some(()),
                UnaryOperator::Delete => None,
                UnaryOperator::Typeof
                    if matches!(
                        unary.argument,
                        Expression::FunctionExpression(_)
                            | Expression::ArrowFunctionExpression(_)
                            | Expression::ClassExpression(_)
                    ) =>
                {
                    Some(())
                }
                _ => self.walk(ev, &unary.argument),
            },
            Expression::ArrayExpression(array) => {
                for element in &array.elements {
                    match element {
                        ArrayExpressionElement::Elision(_)
                        | ArrayExpressionElement::SpreadElement(_) => return None,
                        _ => self.walk(ev, element.to_expression())?,
                    }
                }
                Some(())
            }
            Expression::ObjectExpression(object) => {
                for property in &object.properties {
                    match property {
                        ObjectPropertyKind::SpreadProperty(spread) => {
                            self.walk(ev, &spread.argument)?;
                        }
                        ObjectPropertyKind::ObjectProperty(prop) => {
                            if prop.method || prop.kind != PropertyKind::Init {
                                return None;
                            }
                            if prop.computed {
                                let key_expr = prop.key.as_expression()?;
                                self.walk(ev, key_expr)?;
                                let key_value = self.eval_value(ev, key_expr)?;
                                js_to_string(&key_value)?;
                            }
                            self.walk(ev, &prop.value)?;
                        }
                    }
                }
                Some(())
            }
            Expression::LogicalExpression(logical) => {
                // parity: both operands evaluate eagerly upstream.
                self.walk(ev, &logical.left)?;
                self.walk(ev, &logical.right)
            }
            Expression::BinaryExpression(binary) => {
                self.walk(ev, &binary.left)?;
                self.walk(ev, &binary.right)
            }
            Expression::CallExpression(call) => self.walk_call(ev, call),
            _ => None,
        }
    }

    fn walk_member<'a>(
        &mut self,
        ev: &mut Evaluator<'a, '_>,
        expr: &'a Expression<'a>,
    ) -> Option<()> {
        if let Some((base, parts)) = collect_member_chain(expr)
            && parts.len() >= 2
        {
            self.walk(ev, base)?;
            let base_value = self.eval_value(ev, base)?;
            if self.is_self(&base_value) {
                self.record(parts.join("."));
                return Some(());
            }
            let mut current = base_value;
            for (i, key) in parts.iter().enumerate() {
                current = match ev.member_lookup(&current, key, expr.span()) {
                    Ok(EvalOutcome::Value(v)) => v,
                    _ => return None,
                };
                if self.is_self(&current) && i + 1 < parts.len() {
                    self.record(parts[i + 1..].join("."));
                    return Some(());
                }
            }
            return Some(());
        }
        let (object_expr, key) = match expr {
            Expression::StaticMemberExpression(member) => {
                (&member.object, member.property.name.to_string())
            }
            Expression::ComputedMemberExpression(member) => {
                self.walk(ev, &member.expression)?;
                let key_value = self.eval_value(ev, &member.expression)?;
                (&member.object, js_to_string(&key_value)?)
            }
            _ => return None,
        };
        self.walk(ev, object_expr)?;
        let object = self.eval_value(ev, object_expr)?;
        if self.is_self(&object) {
            self.record_access(&key);
        }
        Some(())
    }

    fn walk_args_only<'a>(
        &mut self,
        ev: &mut Evaluator<'a, '_>,
        call: &'a CallExpression<'a>,
    ) -> Option<()> {
        for argument in &call.arguments {
            match argument {
                Argument::SpreadElement(_) => return None,
                _ => self.walk(ev, argument.to_expression())?,
            }
        }
        Some(())
    }

    fn walk_arrow_call<'a>(
        &mut self,
        ev: &mut Evaluator<'a, '_>,
        key: u32,
        call: &'a CallExpression<'a>,
    ) -> Option<()> {
        let (body, params) = ev.arrow_bodies.get(&key).map(|(b, p)| (*b, p.clone()))?;
        let mut args = Vec::with_capacity(call.arguments.len());
        for argument in &call.arguments {
            match argument {
                Argument::SpreadElement(_) => return None,
                _ => {
                    let expr = argument.to_expression();
                    self.walk(ev, expr)?;
                    args.push(self.eval_value(ev, expr)?);
                }
            }
        }
        let mut saved: Vec<(String, Option<RegistryEntry>)> = Vec::with_capacity(params.len());
        for (i, name) in params.iter().enumerate() {
            let value = args.get(i).cloned().unwrap_or(JsValue::Undefined);
            let previous = ev
                .registry
                .to_mut()
                .identifiers
                .insert(name.clone(), RegistryEntry::Value(value));
            saved.push((name.clone(), previous));
        }
        let result = self.walk(ev, body);
        let identifiers = &mut ev.registry.to_mut().identifiers;
        for (name, previous) in saved.into_iter().rev() {
            match previous {
                Some(entry) => {
                    identifiers.insert(name, entry);
                }
                None => {
                    identifiers.remove(&name);
                }
            }
        }
        result
    }

    fn walk_call<'a>(
        &mut self,
        ev: &mut Evaluator<'a, '_>,
        call: &'a CallExpression<'a>,
    ) -> Option<()> {
        if call.optional {
            return None;
        }
        let callee = unwrap_parens(&call.callee);
        if let Expression::Identifier(id) = callee {
            let name = id.name.as_str();
            if let Some(entry) = ev.registry.identifiers.get(name) {
                return match entry {
                    RegistryEntry::Callable(_) => self.walk_args_only(ev, call),
                    RegistryEntry::Value(_) => None,
                };
            }
            if ev.state.symbol_of(id).is_none() && is_valid_callee(name) {
                return match name {
                    "Math" => None,
                    _ => self.walk_args_only(ev, call),
                };
            }
            self.walk(ev, callee)?;
            return match self.eval_value(ev, callee)? {
                JsValue::Callable(Callable::Stylex(_)) => self.walk_args_only(ev, call),
                JsValue::Callable(Callable::Arrow(key)) => self.walk_arrow_call(ev, key, call),
                _ => None,
            };
        }
        if let Some(member) = callee.as_member_expression() {
            let property =
                member
                    .static_property_name()
                    .map(str::to_string)
                    .or_else(|| match member {
                        oxc_ast::ast::MemberExpression::ComputedMemberExpression(m) => {
                            match &m.expression {
                                Expression::StringLiteral(lit) => Some(lit.value.to_string()),
                                _ => None,
                            }
                        }
                        _ => None,
                    });
            let property = property?;
            if let Expression::Identifier(object) = member.object() {
                let object_name = object.name.as_str();
                let is_member_callable = ev
                    .registry
                    .member_callable(object_name, &property)
                    .is_some();
                if !is_member_callable
                    && is_valid_callee(object_name)
                    && !is_invalid_method(&property)
                {
                    return match crate::eval::methods::lookup_global_static(object_name, &property)
                    {
                        crate::eval::methods::StaticMember::Fn(_) => self.walk_args_only(ev, call),
                        crate::eval::methods::StaticMember::Unknown => None,
                        _ => None,
                    };
                }
                if is_member_callable {
                    return self.walk_args_only(ev, call);
                }
            }
            self.walk(ev, member.object())?;
            let object_value = self.eval_value(ev, member.object())?;
            if matches!(&object_value, JsValue::Proxy(_)) && property == "toString" {
                return self.walk_args_only(ev, call);
            }
            if let JsValue::Obj(obj) = &object_value
                && let Some(JsValue::Callable(callable)) = obj.get(&property)
            {
                match callable.clone() {
                    Callable::Stylex(_) => return self.walk_args_only(ev, call),
                    Callable::Arrow(key) => return self.walk_arrow_call(ev, key, call),
                    Callable::Opaque => {}
                }
            }
            return None;
        }
        None
    }
}

/// The vars-context keyframes closure skips visitor validation upstream;
/// Object.entries shapes strings/arrays/primitives into frame objects instead.
pub fn coerce_vars_keyframes_frames(frames: &EvalValue) -> Result<EvalValue, StylexError> {
    fn entries_object(value: &EvalValue) -> Option<crate::eval::value::JsObjectMap> {
        let mut out = crate::eval::value::JsObjectMap::new();
        match value {
            EvalValue::Str(s) => {
                for (i, unit) in s.encode_utf16().enumerate() {
                    out.insert(
                        i.to_string(),
                        EvalValue::Str(String::from_utf16_lossy(&[unit])),
                    );
                }
            }
            EvalValue::Arr(items) => {
                for (i, v) in items.iter().enumerate() {
                    out.insert(i.to_string(), v.clone());
                }
            }
            EvalValue::Num(_) | EvalValue::Bool(_) => {}
            _ => return None,
        }
        Some(out)
    }
    let frame_entries: Vec<(String, EvalValue)> = match frames {
        EvalValue::Obj(map) => map
            .entries()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        EvalValue::Null | EvalValue::Undefined => {
            return Err(StylexError::upstream_type_crash(
                "a nullish keyframes() argument in a defineVars/createTheme context",
            ));
        }
        other => entries_object(other)
            .expect("non-object frames are strings, arrays or primitives here")
            .entries()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    };
    let mut out = crate::eval::value::JsObjectMap::new();
    for (key, frame) in frame_entries {
        let coerced = match &frame {
            EvalValue::Obj(_) | EvalValue::Null => frame,
            // Object.entries(undefined) crashes exactly like a null frame does.
            EvalValue::Undefined => EvalValue::Null,
            other => EvalValue::Obj(Arc::new(
                entries_object(other).expect("frame values here are strings/arrays/primitives"),
            )),
        };
        out.insert(key, coerced);
    }
    Ok(EvalValue::Obj(Arc::new(out)))
}
