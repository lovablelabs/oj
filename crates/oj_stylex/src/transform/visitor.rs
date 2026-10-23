//! Whole-module transform: call-site discovery in babel visitor order, splice
//! edits, DCE, treeshake compensation. parity: babel-plugin src/index.js:71-386

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use memchr::memmem;
use oxc_ast::ast::{
    Argument, ArrayExpressionElement, BindingPattern, CallExpression, ChainElement, ClassElement,
    Declaration, ExportDefaultDeclarationKind, Expression, ForStatementInit, ForStatementLeft,
    Function, JSXAttribute, JSXAttributeItem, JSXAttributeName, JSXAttributeValue, JSXChild,
    JSXElement, JSXElementName, JSXExpression, JSXFragment, JSXOpeningElement, ModuleExportName,
    ObjectPropertyKind, Program, SimpleAssignmentTarget, Statement, TSNamespaceDeclarationBody,
    VariableDeclaration,
};
use oxc_span::{GetSpan, Span};

use crate::errors::StylexError;
use crate::eval::functions::{
    EvalCallables, StylexCallable, coerce_vars_keyframes_frames, vars_registry,
};
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::eval::{
    EvalOutcome, Evaluator, FunctionRegistry, JsValue, evaluate_stylex_create_arg, from_eval_value,
    into_eval_value, into_js_value, is_nullish, to_eval_value, unwrap_parens, validate_create_arg,
};
use crate::imports::StylexNamedImport;
use crate::jsrt::js_number_to_string;
use crate::module_resolution::{
    FsProvider, canonical_file_path, matches_file_suffix, node_basename, rewritten_import_source,
};
use crate::options::ModuleResolutionType;
use crate::rules::{StylexRule, non_finite_to_tag};
use crate::shared::create::{CreateContext, INLINE_NAMESPACE, compile_atom, compile_namespaces};
use crate::shared::create_theme::{apply_theme_dev_naming, create_theme};
use crate::shared::define_consts::define_consts;
use crate::shared::define_vars::{define_vars, define_vars_core};
use crate::shared::dynamic::{compile_dynamic_namespace, inherit_rules};
use crate::shared::keyframes::keyframes;
use crate::shared::markers::{default_marker_object, define_marker_object};
use crate::shared::nested::{
    define_consts_nested, flatten_nested_consts_config, flatten_nested_overrides_config,
    flatten_nested_string_config, flatten_nested_vars_config, nest_define_vars_js_output,
};
use crate::shared::position_try::{position_try, position_try_shared};
use crate::shared::types::create_css_type;
use crate::shared::view_transition::view_transition_class;
use crate::shared::when::{WhenRelation, when_selector};
use crate::state::{CompileState, RefParent};
use crate::transform::ast_backend::{
    AstPlan, DynamicEntry, HoistValue, InsertOp, JsxOp, PrologueStmt, RemoveOp, SynthExpr,
    SynthStmt,
};
use crate::transform::atoms::{AtomStyle, dynamic_style, static_style};
use crate::transform::dce::{DceAction, compute_vars_to_keep, dce_action};
use crate::transform::js_out::{
    Edit, SpliceMap, apply_edits_tracked, estimate_object_len, inject_call_text, is_identifier_key,
    js_string_literal, print_dynamic_arrow, print_static_chunk, render_span, write_object,
    write_value,
};
use crate::transform::merge::{
    MemberEval, MergeArg, MergeMode, MergePlan, NullableStyle, StyleVarToKeep, StyleVarsCollector,
    plan_legacy_merge, plan_merge,
};

// ---------------------------------------------------------------------------
// Compile-time callables (the create-evaluation FunctionConfig implementations)

pub struct RealCallables;

impl EvalCallables for RealCallables {
    fn call(
        &self,
        callee: &StylexCallable,
        args: &[EvalValue],
        state: &mut CompileState<'_>,
    ) -> Result<EvalValue, StylexError> {
        match callee {
            StylexCallable::Keyframes => {
                let frames = args.first().cloned().unwrap_or(EvalValue::Undefined);
                let (name, rule) = keyframes(&frames, state.options)?;
                state.record_other_injected_rule(rule);
                Ok(EvalValue::Str(name))
            }
            StylexCallable::FirstThatWorks => first_that_works(args),
            StylexCallable::DefaultMarker => {
                Ok(EvalValue::Obj(default_marker_object(state.options).into()))
            }
            StylexCallable::WhenAncestor
            | StylexCallable::WhenDescendant
            | StylexCallable::WhenSiblingBefore
            | StylexCallable::WhenSiblingAfter
            | StylexCallable::WhenAnySibling => {
                let relation = match callee {
                    StylexCallable::WhenAncestor => WhenRelation::Ancestor,
                    StylexCallable::WhenDescendant => WhenRelation::Descendant,
                    StylexCallable::WhenSiblingBefore => WhenRelation::SiblingBefore,
                    StylexCallable::WhenSiblingAfter => WhenRelation::SiblingAfter,
                    _ => WhenRelation::AnySibling,
                };
                let Some(EvalValue::Str(pseudo)) = args.first() else {
                    return Err(StylexError::upstream_type_crash(
                        "a non-string when.* pseudo selector",
                    ));
                };
                when_selector(relation, pseudo, args.get(1), state.options).map(EvalValue::Str)
            }
            StylexCallable::PositionTry => {
                let styles = args.first().cloned().unwrap_or(EvalValue::Undefined);
                let (name, rule) = position_try_shared(&styles, state.options)?;
                state.record_other_injected_rule(rule);
                Ok(EvalValue::Str(name))
            }
            StylexCallable::Conditional => {
                Ok(args.first().cloned().unwrap_or(EvalValue::Undefined))
            }
            StylexCallable::Types(_) => Err(StylexError::unsupported_api("types.*")),
        }
    }
}

/// Theming-visitor FunctionConfig callables: keyframes skips visitor-level
/// validation there, and `types.*` builds compile-time CSSType values.
pub struct VarsCallables;

impl EvalCallables for VarsCallables {
    fn call(
        &self,
        callee: &StylexCallable,
        args: &[EvalValue],
        state: &mut CompileState<'_>,
    ) -> Result<EvalValue, StylexError> {
        match callee {
            StylexCallable::Keyframes => {
                let raw = args.first().cloned().unwrap_or(EvalValue::Undefined);
                let frames = coerce_vars_keyframes_frames(&raw)?;
                let (name, rule) = keyframes(&frames, state.options)?;
                state.record_other_injected_rule(rule);
                Ok(EvalValue::Str(name))
            }
            StylexCallable::Types(kind) => create_css_type(kind, args),
            StylexCallable::FirstThatWorks => first_that_works(args),
            other => RealCallables.call(other, args, state),
        }
    }
}

// parity: shared/stylex-first-that-works.js (exact truncation + reduce shape)
fn is_var_arg(value: &EvalValue) -> bool {
    let EvalValue::Str(s) = value else {
        return false;
    };
    let Some(body) = s.strip_prefix("var(--").and_then(|r| r.strip_suffix(')')) else {
        return false;
    };
    !body.is_empty()
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn first_that_works(args: &[EvalValue]) -> Result<EvalValue, StylexError> {
    let Some(first_var) = args.iter().position(is_var_arg) else {
        return Ok(EvalValue::Arr(args.iter().rev().cloned().collect()));
    };
    let priorities: Vec<EvalValue> = args[..first_var].iter().rev().cloned().collect();
    let rest = &args[first_var..];
    let first_non_var = rest.iter().position(|a| !is_var_arg(a));
    let cut = first_non_var.map_or(rest.len(), |i| (i + 1).min(rest.len()));
    let var_parts: Vec<&EvalValue> = rest[..cut].iter().rev().collect();

    let mut so_far = String::new();
    for part in var_parts {
        let name: String = match part {
            EvalValue::Str(s) if is_var_arg(part) => s[4..s.len() - 1].to_string(),
            EvalValue::Str(s) => s.clone(),
            EvalValue::Num(n) if !so_far.is_empty() => js_number_to_string(*n),
            _ => {
                return Err(StylexError::upstream_type_crash(
                    "a non-string firstThatWorks argument",
                ));
            }
        };
        so_far = if !so_far.is_empty() {
            format!("var({name}, {so_far})")
        } else if name.starts_with("--") {
            format!("var({name})")
        } else {
            name
        };
    }
    let mut out = vec![EvalValue::Str(so_far)];
    out.extend(priorities);
    if out.len() == 1 {
        Ok(out.pop().expect("one element"))
    } else {
        Ok(EvalValue::Arr(out))
    }
}

// ---------------------------------------------------------------------------
// Call-site classification

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallKind {
    Create,
    Keyframes,
    PositionTry,
    ViewTransitionClass,
    DefaultMarker,
    DefineMarker,
    DefineVars,
    DefineConsts,
    CreateTheme,
    DefineVarsNested,
    DefineConstsNested,
    CreateThemeNested,
    Props,
    Attrs,
    LegacyMerge,
}

// parity: each visitor's `property.type === 'Identifier'` check admits
// computed members with identifier keys (`stylex[create]`), never literals.
fn member_prop_name<'b>(callee: &'b Expression<'_>) -> Option<(&'b str, &'b str)> {
    match unwrap_parens(callee) {
        Expression::StaticMemberExpression(m) => match unwrap_parens(&m.object) {
            Expression::Identifier(obj) => Some((obj.name.as_str(), m.property.name.as_str())),
            _ => None,
        },
        Expression::ComputedMemberExpression(m) => {
            match (unwrap_parens(&m.object), &m.expression) {
                (Expression::Identifier(obj), Expression::Identifier(prop)) => {
                    Some((obj.name.as_str(), prop.name.as_str()))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn call_kind(call: &CallExpression<'_>, state: &CompileState<'_>) -> Option<CallKind> {
    let imports = &state.imports;
    if let Expression::Identifier(id) = unwrap_parens(&call.callee) {
        let name = id.name.as_str();
        if let Some(binding) = imports.named_binding(name) {
            return Some(match binding {
                StylexNamedImport::Create => CallKind::Create,
                StylexNamedImport::Keyframes => CallKind::Keyframes,
                StylexNamedImport::PositionTry => CallKind::PositionTry,
                StylexNamedImport::ViewTransitionClass => CallKind::ViewTransitionClass,
                StylexNamedImport::DefaultMarker => CallKind::DefaultMarker,
                StylexNamedImport::DefineMarker => CallKind::DefineMarker,
                StylexNamedImport::DefineVars => CallKind::DefineVars,
                StylexNamedImport::DefineConsts => CallKind::DefineConsts,
                StylexNamedImport::CreateTheme => CallKind::CreateTheme,
                StylexNamedImport::DefineVarsNested => CallKind::DefineVarsNested,
                StylexNamedImport::DefineConstsNested => CallKind::DefineConstsNested,
                StylexNamedImport::CreateThemeNested => CallKind::CreateThemeNested,
                StylexNamedImport::Props => CallKind::Props,
                StylexNamedImport::Attrs => CallKind::Attrs,
                _ => return None,
            });
        }
        if imports.is_stylex_namespace(name) {
            return Some(CallKind::LegacyMerge);
        }
        return None;
    }
    let (object, property) = member_prop_name(&call.callee)?;
    if !imports.is_stylex_namespace(object) {
        return None;
    }
    member_api_kind(property)
}

fn member_api_kind(property: &str) -> Option<CallKind> {
    Some(match property {
        "create" => CallKind::Create,
        "keyframes" => CallKind::Keyframes,
        "positionTry" => CallKind::PositionTry,
        "viewTransitionClass" => CallKind::ViewTransitionClass,
        "defaultMarker" => CallKind::DefaultMarker,
        "defineMarker" => CallKind::DefineMarker,
        "defineVars" => CallKind::DefineVars,
        "defineConsts" => CallKind::DefineConsts,
        "createTheme" => CallKind::CreateTheme,
        "unstable_defineVarsNested" => CallKind::DefineVarsNested,
        "unstable_defineConstsNested" => CallKind::DefineConstsNested,
        "unstable_createThemeNested" => CallKind::CreateThemeNested,
        "props" => CallKind::Props,
        "attrs" => CallKind::Attrs,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Generic AST walk with the hooks the three passes need

/// Where a top-level declarator sits in its statement; DCE plans removal
/// edits per statement so sibling-declarator spans stay disjoint.
#[derive(Debug, Clone)]
struct DeclRemoval {
    stmt_span: Span,
    decl_index: usize,
    decl_spans: Rc<[Span]>,
}

#[derive(Debug, Clone)]
struct DeclCtx {
    name: Option<String>,
    top_level: bool,
    exported: bool,
    removal: Option<DeclRemoval>,
}

#[derive(Clone, Copy)]
enum ExprParent<'c> {
    Statement,
    Declarator(&'c DeclCtx),
    JsxSpread(Span),
    Other,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Flow {
    Walk,
    Skip,
    /// Callee replaced, arguments still live (the atoms dynamic form).
    ArgsOnly,
}

enum MemberProp<'a> {
    Static(&'a str),
    Computed(&'a Expression<'a>),
}

struct MemberInfo<'a> {
    object: &'a Expression<'a>,
    prop: MemberProp<'a>,
    optional: bool,
    span: Span,
    node_id: oxc_syntax::node::NodeId,
    /// The member as an `Expression` when one wraps it (assignment-target
    /// members have none); the collector's lazy evaluate needs it.
    expr: Option<&'a Expression<'a>>,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum MemberFlow {
    Walk,
    SkipObject,
}

trait Hooks<'a> {
    fn done(&self) -> bool;
    /// False when no hook of this pass can fire inside `span`: the walker
    /// skips the subtree.
    fn wants(&self, _span: Span) -> bool {
        true
    }
    fn call(&mut self, _call: &'a CallExpression<'a>, _parent: ExprParent<'_>) -> Flow {
        Flow::Walk
    }
    fn jsx_opening(&mut self, _el: &'a JSXOpeningElement<'a>) {}
    fn jsx_attribute(&mut self, _attr: &'a JSXAttribute<'a>) -> Flow {
        Flow::Walk
    }
    fn identifier(&mut self, _name: &str, _span: Span) {}
    fn member(&mut self, _info: &MemberInfo<'a>) -> MemberFlow {
        MemberFlow::Walk
    }
    fn export_local(&mut self, _name: &str, _span: Span) {}
}

/// Sorted source offsets at which a pass's hooks can fire.
struct SiteIndex {
    starts: Vec<u32>,
}

impl SiteIndex {
    fn any_in(&self, span: Span) -> bool {
        let i = self.starts.partition_point(|&s| s < span.start);
        i < self.starts.len() && self.starts[i] < span.end
    }
}

/// Pass A acts on `<local>(…)` for the non-props named imports and on
/// `ns.<api>(…)`; `ns.props(…)`, `ns.attrs(…)` and bare `ns(…)` it walks past.
fn pass_a_index(state: &CompileState<'_>, source: &str) -> SiteIndex {
    let imports = &state.imports;
    let names: Vec<&str> = imports
        .named
        .iter()
        .filter(|(_, b)| !matches!(b, StylexNamedImport::Props | StylexNamedImport::Attrs))
        .map(|(name, _)| name.as_str())
        .chain(imports.stylex_namespaces.iter().map(String::as_str))
        .collect();
    let mut starts = state.reference_starts_where(&names, |name, site| {
        if !imports.is_stylex_namespace(name) {
            return true;
        }
        match site.parent() {
            RefParent::Member {
                property: Some(property),
            } => !matches!(
                member_api_kind(property),
                None | Some(CallKind::Props | CallKind::Attrs)
            ),
            _ => false,
        }
    });
    // A JSX attribute name is no reference; its source text stands in.
    if let Some(sx) = state.options.sx_prop_name.as_deref() {
        starts.extend(memmem::find_iter(source.as_bytes(), sx.as_bytes()).map(|i| i as u32));
        starts.sort_unstable();
    }
    SiteIndex { starts }
}

fn pass_b1_index(
    state: &CompileState<'_>,
    style_map: &BTreeMap<String, Arc<JsObjectMap>>,
) -> SiteIndex {
    let names: Vec<&str> = style_map.keys().map(String::as_str).collect();
    SiteIndex {
        starts: state.reference_starts(&names),
    }
}

fn pass_b2_index(state: &CompileState<'_>, sx_sites: &[SxSite<'_>]) -> SiteIndex {
    let imports = &state.imports;
    let names: Vec<&str> = imports
        .named
        .iter()
        .filter(|(_, b)| matches!(b, StylexNamedImport::Props | StylexNamedImport::Attrs))
        .map(|(name, _)| name.as_str())
        .chain(imports.stylex_namespaces.iter().map(String::as_str))
        .collect();
    let mut starts = state.reference_starts(&names);
    starts.extend(sx_sites.iter().map(|s| s.attr_span.start));
    starts.sort_unstable();
    SiteIndex { starts }
}

fn walk_program<'a, H: Hooks<'a>>(hooks: &mut H, program: &'a Program<'a>) {
    for statement in &program.body {
        walk_top_statement(hooks, statement);
        if hooks.done() {
            return;
        }
    }
}

fn walk_top_statement<'a, H: Hooks<'a>>(hooks: &mut H, statement: &'a Statement<'a>) {
    match statement {
        Statement::ExportNamedDeclaration(export) => {
            for specifier in &export.specifiers {
                if specifier.export_kind.is_type() {
                    continue;
                }
                match &specifier.local {
                    ModuleExportName::IdentifierReference(id) => {
                        hooks.export_local(id.name.as_str(), id.span);
                    }
                    ModuleExportName::IdentifierName(id) => {
                        hooks.export_local(id.name.as_str(), id.span);
                    }
                    ModuleExportName::StringLiteral(_) => {}
                }
            }
        }
        _ if !hooks.wants(statement.span()) => {}
        Statement::VariableDeclaration(decl) => {
            walk_variable_declaration(hooks, decl, Some((false, statement.span())));
        }
        Statement::ExportDeclaration(export) => match &export.declaration {
            Declaration::VariableDeclaration(decl) => {
                walk_variable_declaration(hooks, decl, Some((true, statement.span())));
            }
            other => walk_declaration(hooks, other),
        },
        other => walk_statement(hooks, other),
    }
}

fn walk_statement<'a, H: Hooks<'a>>(hooks: &mut H, statement: &'a Statement<'a>) {
    if hooks.done() || !hooks.wants(statement.span()) {
        return;
    }
    match statement {
        Statement::BlockStatement(block) => {
            for s in &block.body {
                walk_statement(hooks, s);
            }
        }
        Statement::BreakStatement(_)
        | Statement::ContinueStatement(_)
        | Statement::DebuggerStatement(_)
        | Statement::EmptyStatement(_) => {}
        Statement::DoWhileStatement(s) => {
            walk_statement(hooks, &s.body);
            walk_expression(hooks, &s.test, ExprParent::Other);
        }
        Statement::ExpressionStatement(s) => {
            walk_expression(hooks, &s.expression, ExprParent::Statement);
        }
        Statement::ForInStatement(s) => {
            walk_for_left(hooks, &s.left);
            walk_expression(hooks, &s.right, ExprParent::Other);
            walk_statement(hooks, &s.body);
        }
        Statement::ForOfStatement(s) => {
            walk_for_left(hooks, &s.left);
            walk_expression(hooks, &s.right, ExprParent::Other);
            walk_statement(hooks, &s.body);
        }
        Statement::ForStatement(s) => {
            match &s.init {
                Some(ForStatementInit::VariableDeclaration(decl)) => {
                    walk_variable_declaration(hooks, decl, None);
                }
                Some(init) => {
                    if let Some(expr) = init.as_expression() {
                        walk_expression(hooks, expr, ExprParent::Other);
                    }
                }
                None => {}
            }
            if let Some(test) = &s.test {
                walk_expression(hooks, test, ExprParent::Other);
            }
            if let Some(update) = &s.update {
                walk_expression(hooks, update, ExprParent::Other);
            }
            walk_statement(hooks, &s.body);
        }
        Statement::IfStatement(s) => {
            walk_expression(hooks, &s.test, ExprParent::Other);
            walk_statement(hooks, &s.consequent);
            if let Some(alt) = &s.alternate {
                walk_statement(hooks, alt);
            }
        }
        Statement::LabeledStatement(s) => walk_statement(hooks, &s.body),
        Statement::ReturnStatement(s) => {
            if let Some(argument) = &s.argument {
                walk_expression(hooks, argument, ExprParent::Other);
            }
        }
        Statement::SwitchStatement(s) => {
            walk_expression(hooks, &s.discriminant, ExprParent::Other);
            for case in &s.cases {
                if let Some(test) = &case.test {
                    walk_expression(hooks, test, ExprParent::Other);
                }
                for st in &case.consequent {
                    walk_statement(hooks, st);
                }
            }
        }
        Statement::ThrowStatement(s) => walk_expression(hooks, &s.argument, ExprParent::Other),
        Statement::TryStatement(s) => {
            for st in &s.block.body {
                walk_statement(hooks, st);
            }
            if let Some(handler) = &s.handler {
                for st in &handler.body.body {
                    walk_statement(hooks, st);
                }
            }
            if let Some(finalizer) = &s.finalizer {
                for st in &finalizer.body {
                    walk_statement(hooks, st);
                }
            }
        }
        Statement::WhileStatement(s) => {
            walk_expression(hooks, &s.test, ExprParent::Other);
            walk_statement(hooks, &s.body);
        }
        Statement::WithStatement(s) => {
            walk_expression(hooks, &s.object, ExprParent::Other);
            walk_statement(hooks, &s.body);
        }
        Statement::VariableDeclaration(decl) => walk_variable_declaration(hooks, decl, None),
        Statement::FunctionDeclaration(f) => walk_function(hooks, f),
        Statement::ClassDeclaration(class) => walk_class(hooks, class),
        Statement::TSNamespaceDeclaration(ns) => {
            if let TSNamespaceDeclarationBody::TSModuleBlock(block) = &ns.body {
                for st in &block.body {
                    walk_statement(hooks, st);
                }
            }
        }
        Statement::TSEnumDeclaration(e) => {
            for member in &e.body.members {
                if let Some(init) = &member.initializer {
                    walk_expression(hooks, init, ExprParent::Other);
                }
            }
        }
        Statement::TSExportAssignment(e) => {
            walk_expression(hooks, &e.expression, ExprParent::Other);
        }
        Statement::ExportDefaultDeclaration(export) => match &export.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(f) => walk_function(hooks, f),
            ExportDefaultDeclarationKind::ClassDeclaration(class) => walk_class(hooks, class),
            other => {
                if let Some(expr) = other.as_expression() {
                    walk_expression(hooks, expr, ExprParent::Other);
                }
            }
        },
        Statement::ExportDeclaration(export) => walk_declaration(hooks, &export.declaration),
        _ => {}
    }
}

fn walk_declaration<'a, H: Hooks<'a>>(hooks: &mut H, declaration: &'a Declaration<'a>) {
    match declaration {
        Declaration::VariableDeclaration(decl) => walk_variable_declaration(hooks, decl, None),
        Declaration::FunctionDeclaration(f) => walk_function(hooks, f),
        Declaration::ClassDeclaration(class) => walk_class(hooks, class),
        _ => {}
    }
}

fn walk_for_left<'a, H: Hooks<'a>>(hooks: &mut H, left: &'a ForStatementLeft<'a>) {
    match left {
        ForStatementLeft::VariableDeclaration(decl) => {
            walk_variable_declaration(hooks, decl, None);
        }
        other => {
            if let Some(target) = other.as_assignment_target() {
                walk_assignment_target(hooks, target);
            }
        }
    }
}

fn walk_variable_declaration<'a, H: Hooks<'a>>(
    hooks: &mut H,
    decl: &'a VariableDeclaration<'a>,
    top: Option<(bool, Span)>,
) {
    let decl_spans: Option<Rc<[Span]>> =
        top.map(|_| decl.declarations.iter().map(|d| d.span).collect());
    for (i, declarator) in decl.declarations.iter().enumerate() {
        if !hooks.wants(declarator.span) {
            continue;
        }
        let ctx = DeclCtx {
            name: declarator
                .id
                .get_binding_identifier()
                .map(|id| id.name.to_string()),
            top_level: top.is_some(),
            exported: top.is_some_and(|(exported, _)| exported),
            removal: top.map(|(_, stmt_span)| DeclRemoval {
                stmt_span,
                decl_index: i,
                decl_spans: Rc::clone(decl_spans.as_ref().expect("built when top is Some")),
            }),
        };
        walk_binding_pattern(hooks, &declarator.id);
        if let Some(init) = &declarator.init {
            walk_expression(hooks, init, ExprParent::Declarator(&ctx));
        }
        if hooks.done() {
            return;
        }
    }
}

fn walk_binding_pattern<'a, H: Hooks<'a>>(hooks: &mut H, pattern: &'a BindingPattern<'a>) {
    match pattern {
        BindingPattern::BindingIdentifier(_) => {}
        BindingPattern::ObjectPattern(p) => {
            for prop in &p.properties {
                if prop.computed
                    && let Some(key) = prop.key.as_expression()
                {
                    walk_expression(hooks, key, ExprParent::Other);
                }
                walk_binding_pattern(hooks, &prop.value);
            }
            if let Some(rest) = &p.rest {
                walk_binding_pattern(hooks, &rest.argument);
            }
        }
        BindingPattern::ArrayPattern(p) => {
            for element in p.elements.iter().flatten() {
                walk_binding_pattern(hooks, element);
            }
            if let Some(rest) = &p.rest {
                walk_binding_pattern(hooks, &rest.argument);
            }
        }
        BindingPattern::AssignmentPattern(p) => {
            walk_binding_pattern(hooks, &p.left);
            walk_expression(hooks, &p.right, ExprParent::Other);
        }
    }
}

fn walk_function<'a, H: Hooks<'a>>(hooks: &mut H, function: &'a Function<'a>) {
    if !hooks.wants(function.span) {
        return;
    }
    for param in &function.params.items {
        walk_binding_pattern(hooks, &param.pattern);
    }
    if let Some(rest) = &function.params.rest {
        walk_binding_pattern(hooks, &rest.rest.argument);
    }
    if let Some(body) = &function.body {
        for statement in &body.statements {
            walk_statement(hooks, statement);
        }
    }
}

fn walk_class<'a, H: Hooks<'a>>(hooks: &mut H, class: &'a oxc_ast::ast::Class<'a>) {
    if !hooks.wants(class.span) {
        return;
    }
    if let Some(heritage) = &class.heritage {
        walk_expression(hooks, &heritage.expression, ExprParent::Other);
    }
    for element in &class.body.body {
        match element {
            ClassElement::MethodDefinition(m) => {
                if m.computed
                    && let Some(key) = m.key.as_expression()
                {
                    walk_expression(hooks, key, ExprParent::Other);
                }
                walk_function(hooks, &m.value);
            }
            ClassElement::PropertyDefinition(p) => {
                if p.computed
                    && let Some(key) = p.key.as_expression()
                {
                    walk_expression(hooks, key, ExprParent::Other);
                }
                if let Some(value) = &p.value {
                    walk_expression(hooks, value, ExprParent::Other);
                }
            }
            ClassElement::AccessorProperty(p) => {
                if let Some(value) = &p.value {
                    walk_expression(hooks, value, ExprParent::Other);
                }
            }
            ClassElement::StaticBlock(block) => {
                for statement in &block.body {
                    walk_statement(hooks, statement);
                }
            }
            ClassElement::TSIndexSignature(_) => {}
        }
    }
}

fn walk_assignment_target<'a, H: Hooks<'a>>(
    hooks: &mut H,
    target: &'a oxc_ast::ast::AssignmentTarget<'a>,
) {
    use oxc_ast::ast::AssignmentTarget as At;
    match target {
        At::AssignmentTargetIdentifier(_) => {}
        At::StaticMemberExpression(m) => {
            walk_member_static(hooks, m, None);
        }
        At::ComputedMemberExpression(m) => {
            walk_member_computed(hooks, m, None);
        }
        At::PrivateFieldExpression(m) => {
            walk_expression(hooks, &m.object, ExprParent::Other);
        }
        At::TSAsExpression(e) => walk_expression(hooks, &e.expression, ExprParent::Other),
        At::TSSatisfiesExpression(e) => walk_expression(hooks, &e.expression, ExprParent::Other),
        At::TSNonNullExpression(e) => walk_expression(hooks, &e.expression, ExprParent::Other),
        At::TSTypeAssertion(e) => walk_expression(hooks, &e.expression, ExprParent::Other),
        At::ArrayAssignmentTarget(arr) => {
            for element in arr.elements.iter().flatten() {
                walk_assignment_target_maybe(hooks, element);
            }
            if let Some(rest) = &arr.rest {
                walk_assignment_target(hooks, &rest.target);
            }
        }
        At::ObjectAssignmentTarget(obj) => {
            use oxc_ast::ast::AssignmentTargetProperty as Atp;
            for prop in &obj.properties {
                match prop {
                    Atp::AssignmentTargetPropertyIdentifier(p) => {
                        if let Some(init) = &p.init {
                            walk_expression(hooks, init, ExprParent::Other);
                        }
                    }
                    Atp::AssignmentTargetPropertyProperty(p) => {
                        if p.computed
                            && let Some(key) = p.name.as_expression()
                        {
                            walk_expression(hooks, key, ExprParent::Other);
                        }
                        walk_assignment_target_maybe(hooks, &p.binding);
                    }
                }
            }
            if let Some(rest) = &obj.rest {
                walk_assignment_target(hooks, &rest.target);
            }
        }
    }
}

fn walk_assignment_target_maybe<'a, H: Hooks<'a>>(
    hooks: &mut H,
    target: &'a oxc_ast::ast::AssignmentTargetMaybeDefault<'a>,
) {
    use oxc_ast::ast::AssignmentTargetMaybeDefault as Atd;
    match target {
        Atd::AssignmentTargetWithDefault(t) => {
            walk_assignment_target(hooks, &t.binding);
            walk_expression(hooks, &t.init, ExprParent::Other);
        }
        other => {
            if let Some(target) = other.as_assignment_target() {
                walk_assignment_target(hooks, target);
            }
        }
    }
}

fn walk_member_static<'a, H: Hooks<'a>>(
    hooks: &mut H,
    m: &'a oxc_ast::ast::StaticMemberExpression<'a>,
    expr: Option<&'a Expression<'a>>,
) {
    let info = MemberInfo {
        object: &m.object,
        prop: MemberProp::Static(m.property.name.as_str()),
        optional: m.optional,
        span: m.span,
        node_id: m.node_id.get(),
        expr,
    };
    if hooks.member(&info) == MemberFlow::Walk {
        walk_expression(hooks, &m.object, ExprParent::Other);
    }
}

fn walk_member_computed<'a, H: Hooks<'a>>(
    hooks: &mut H,
    m: &'a oxc_ast::ast::ComputedMemberExpression<'a>,
    expr: Option<&'a Expression<'a>>,
) {
    let info = MemberInfo {
        object: &m.object,
        prop: MemberProp::Computed(&m.expression),
        optional: m.optional,
        span: m.span,
        node_id: m.node_id.get(),
        expr,
    };
    if hooks.member(&info) == MemberFlow::Walk {
        walk_expression(hooks, &m.object, ExprParent::Other);
    }
    walk_expression(hooks, &m.expression, ExprParent::Other);
}

fn walk_call<'a, H: Hooks<'a>>(
    hooks: &mut H,
    call: &'a CallExpression<'a>,
    parent: ExprParent<'_>,
) {
    if !hooks.wants(call.span) {
        return;
    }
    match hooks.call(call, parent) {
        Flow::Skip => return,
        Flow::Walk => walk_expression(hooks, &call.callee, ExprParent::Other),
        Flow::ArgsOnly => {}
    }
    walk_arguments(hooks, &call.arguments);
}

fn walk_arguments<'a, H: Hooks<'a>>(hooks: &mut H, arguments: &'a [Argument<'a>]) {
    for argument in arguments {
        match argument {
            Argument::SpreadElement(spread) => {
                walk_expression(hooks, &spread.argument, ExprParent::Other);
            }
            other => {
                if let Some(expr) = other.as_expression() {
                    walk_expression(hooks, expr, ExprParent::Other);
                }
            }
        }
    }
}

fn walk_expression<'a, H: Hooks<'a>>(
    hooks: &mut H,
    expr: &'a Expression<'a>,
    parent: ExprParent<'_>,
) {
    if hooks.done() {
        return;
    }
    match expr {
        Expression::ParenthesizedExpression(e) => walk_expression(hooks, &e.expression, parent),
        Expression::BooleanLiteral(_)
        | Expression::NullLiteral(_)
        | Expression::NumericLiteral(_)
        | Expression::BigIntLiteral(_)
        | Expression::RegExpLiteral(_)
        | Expression::StringLiteral(_)
        | Expression::ThisExpression(_)
        | Expression::Super(_)
        | Expression::ImportMeta(_)
        | Expression::NewTarget(_) => {}
        Expression::TemplateLiteral(t) => {
            for e in &t.expressions {
                walk_expression(hooks, e, ExprParent::Other);
            }
        }
        Expression::Identifier(id) => hooks.identifier(id.name.as_str(), id.span),
        Expression::ArrayExpression(arr) => {
            if !hooks.wants(arr.span) {
                return;
            }
            for element in &arr.elements {
                match element {
                    ArrayExpressionElement::Elision(_) => {}
                    ArrayExpressionElement::SpreadElement(spread) => {
                        walk_expression(hooks, &spread.argument, ExprParent::Other);
                    }
                    other => {
                        if let Some(e) = other.as_expression() {
                            walk_expression(hooks, e, ExprParent::Other);
                        }
                    }
                }
            }
        }
        Expression::ArrowFunctionExpression(arrow) => {
            if !hooks.wants(arrow.span) {
                return;
            }
            for param in &arrow.params.items {
                walk_binding_pattern(hooks, &param.pattern);
            }
            if let Some(rest) = &arrow.params.rest {
                walk_binding_pattern(hooks, &rest.rest.argument);
            }
            match &arrow.body {
                oxc_ast::ast::ArrowFunctionBody::FunctionBody(body) => {
                    for statement in &body.statements {
                        walk_statement(hooks, statement);
                    }
                }
                // Babel has no statement around an expression body, so a call
                // there never sees an ExpressionStatement parent.
                other => {
                    if let Some(e) = other.as_expression() {
                        walk_expression(hooks, e, ExprParent::Other);
                    }
                }
            }
        }
        Expression::AssignmentExpression(a) => {
            walk_assignment_target(hooks, &a.left);
            walk_expression(hooks, &a.right, ExprParent::Other);
        }
        Expression::AwaitExpression(a) => walk_expression(hooks, &a.argument, ExprParent::Other),
        Expression::BinaryExpression(b) => {
            walk_expression(hooks, &b.left, ExprParent::Other);
            walk_expression(hooks, &b.right, ExprParent::Other);
        }
        Expression::CallExpression(call) => walk_call(hooks, call, parent),
        Expression::ChainExpression(chain) => walk_chain(hooks, &chain.expression),
        Expression::ClassExpression(class) => walk_class(hooks, class),
        Expression::ConditionalExpression(c) => {
            walk_expression(hooks, &c.test, ExprParent::Other);
            walk_expression(hooks, &c.consequent, ExprParent::Other);
            walk_expression(hooks, &c.alternate, ExprParent::Other);
        }
        Expression::FunctionExpression(f) => walk_function(hooks, f),
        Expression::ImportExpression(i) => {
            walk_expression(hooks, &i.source, ExprParent::Other);
            if let Some(options) = &i.options {
                walk_expression(hooks, options, ExprParent::Other);
            }
        }
        Expression::LogicalExpression(l) => {
            walk_expression(hooks, &l.left, ExprParent::Other);
            walk_expression(hooks, &l.right, ExprParent::Other);
        }
        Expression::NewExpression(n) => {
            walk_expression(hooks, &n.callee, ExprParent::Other);
            walk_arguments(hooks, &n.arguments);
        }
        Expression::ObjectExpression(o) => {
            if !hooks.wants(o.span) {
                return;
            }
            for property in &o.properties {
                match property {
                    ObjectPropertyKind::ObjectProperty(p) => {
                        if p.computed
                            && let Some(key) = p.key.as_expression()
                        {
                            walk_expression(hooks, key, ExprParent::Other);
                        }
                        walk_expression(hooks, &p.value, ExprParent::Other);
                    }
                    ObjectPropertyKind::SpreadProperty(spread) => {
                        walk_expression(hooks, &spread.argument, ExprParent::Other);
                    }
                }
            }
        }
        Expression::SequenceExpression(s) => {
            for e in &s.expressions {
                walk_expression(hooks, e, ExprParent::Other);
            }
        }
        Expression::TaggedTemplateExpression(t) => {
            walk_expression(hooks, &t.tag, ExprParent::Other);
            for e in &t.quasi.expressions {
                walk_expression(hooks, e, ExprParent::Other);
            }
        }
        Expression::UnaryExpression(u) => walk_expression(hooks, &u.argument, ExprParent::Other),
        Expression::UpdateExpression(u) => match &u.argument {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(_) => {}
            SimpleAssignmentTarget::StaticMemberExpression(m) => {
                walk_member_static(hooks, m, None);
            }
            SimpleAssignmentTarget::ComputedMemberExpression(m) => {
                walk_member_computed(hooks, m, None);
            }
            _ => {}
        },
        Expression::YieldExpression(y) => {
            if let Some(argument) = &y.argument {
                walk_expression(hooks, argument, ExprParent::Other);
            }
        }
        Expression::PrivateInExpression(p) => {
            walk_expression(hooks, &p.right, ExprParent::Other);
        }
        Expression::JSXElement(el) => walk_jsx_element(hooks, el),
        Expression::JSXFragment(fragment) => walk_jsx_fragment(hooks, fragment),
        Expression::TSAsExpression(e) => walk_expression(hooks, &e.expression, ExprParent::Other),
        Expression::TSSatisfiesExpression(e) => {
            walk_expression(hooks, &e.expression, ExprParent::Other);
        }
        Expression::TSTypeAssertion(e) => {
            walk_expression(hooks, &e.expression, ExprParent::Other);
        }
        Expression::TSNonNullExpression(e) => {
            walk_expression(hooks, &e.expression, ExprParent::Other);
        }
        Expression::TSInstantiationExpression(e) => {
            walk_expression(hooks, &e.expression, ExprParent::Other);
        }
        Expression::V8IntrinsicExpression(e) => walk_arguments(hooks, &e.arguments),
        Expression::StaticMemberExpression(m) => walk_member_static(hooks, m, Some(expr)),
        Expression::ComputedMemberExpression(m) => walk_member_computed(hooks, m, Some(expr)),
        Expression::PrivateFieldExpression(m) => {
            walk_expression(hooks, &m.object, ExprParent::Other);
        }
    }
}

// Optional-chain tails are OptionalMemberExpression/OptionalCallExpression in
// babel, so member/call hooks never fire for the links themselves.
fn walk_chain<'a, H: Hooks<'a>>(hooks: &mut H, element: &'a ChainElement<'a>) {
    match element {
        ChainElement::CallExpression(call) => {
            walk_expression(hooks, &call.callee, ExprParent::Other);
            walk_arguments(hooks, &call.arguments);
        }
        ChainElement::TSNonNullExpression(e) => {
            walk_expression(hooks, &e.expression, ExprParent::Other);
        }
        ChainElement::StaticMemberExpression(m) => {
            walk_expression(hooks, &m.object, ExprParent::Other);
        }
        ChainElement::ComputedMemberExpression(m) => {
            walk_expression(hooks, &m.object, ExprParent::Other);
            walk_expression(hooks, &m.expression, ExprParent::Other);
        }
        ChainElement::PrivateFieldExpression(m) => {
            walk_expression(hooks, &m.object, ExprParent::Other);
        }
    }
}

fn walk_jsx_element<'a, H: Hooks<'a>>(hooks: &mut H, el: &'a JSXElement<'a>) {
    if !hooks.wants(el.span) {
        return;
    }
    hooks.jsx_opening(&el.opening_element);
    for item in &el.opening_element.attributes {
        match item {
            JSXAttributeItem::Attribute(attr) => {
                if hooks.jsx_attribute(attr) == Flow::Walk
                    && let Some(value) = &attr.value
                {
                    walk_jsx_attribute_value(hooks, value);
                }
            }
            JSXAttributeItem::SpreadAttribute(spread) => {
                walk_expression(hooks, &spread.argument, ExprParent::JsxSpread(spread.span));
            }
        }
    }
    walk_jsx_children(hooks, &el.children);
}

fn walk_jsx_fragment<'a, H: Hooks<'a>>(hooks: &mut H, fragment: &'a JSXFragment<'a>) {
    if !hooks.wants(fragment.span) {
        return;
    }
    walk_jsx_children(hooks, &fragment.children);
}

fn walk_jsx_children<'a, H: Hooks<'a>>(hooks: &mut H, children: &'a [JSXChild<'a>]) {
    for child in children {
        match child {
            JSXChild::Text(_) => {}
            JSXChild::Element(el) => walk_jsx_element(hooks, el),
            JSXChild::Fragment(fragment) => walk_jsx_fragment(hooks, fragment),
            JSXChild::ExpressionContainer(container) => {
                walk_jsx_expression(hooks, &container.expression);
            }
            JSXChild::Spread(spread) => {
                walk_expression(hooks, &spread.expression, ExprParent::Other);
            }
        }
    }
}

fn walk_jsx_attribute_value<'a, H: Hooks<'a>>(hooks: &mut H, value: &'a JSXAttributeValue<'a>) {
    match value {
        JSXAttributeValue::StringLiteral(_) => {}
        JSXAttributeValue::ExpressionContainer(container) => {
            walk_jsx_expression(hooks, &container.expression);
        }
        JSXAttributeValue::Element(el) => walk_jsx_element(hooks, el),
        JSXAttributeValue::Fragment(fragment) => walk_jsx_fragment(hooks, fragment),
    }
}

fn walk_jsx_expression<'a, H: Hooks<'a>>(hooks: &mut H, expression: &'a JSXExpression<'a>) {
    match expression {
        JSXExpression::EmptyExpression(_) => {}
        other => {
            if let Some(expr) = other.as_expression() {
                walk_expression(hooks, expr, ExprParent::Other);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Module transform driver

pub struct TransformOutput {
    pub code: String,
    pub modified: bool,
    /// Generated-to-original positions, only when the caller asked for them.
    pub splice_map: Option<SpliceMap>,
    /// (var name, compiled namespaces) per create call, pre-DCE, in order.
    pub create_objects: Vec<(Option<String>, Arc<JsObjectMap>)>,
    /// The same edits as span-keyed AST mutations (transform_program backend).
    pub plan: AstPlan,
}

struct CreateSite {
    span: Span,
    edit_idx: usize,
    /// The create replacement's plan entry (program-level sites only); DCE
    /// pruning rewrites it in lockstep with the splice edit.
    plan_idx: Option<usize>,
    /// Hoisted dynamic static-chunk consts; they survive DCE (upstream leaves
    /// hoistExpression output behind when the create declarator is removed).
    hoist_edit_idxs: Vec<usize>,
    compiled: Arc<JsObjectMap>,
    /// namespace → printed arrow text for dynamic (fns) namespaces.
    dynamic: Vec<(String, String)>,
    /// The structured twins of `dynamic` for the AST plan.
    dynamic_entries: Vec<DynamicEntry>,
    style_var: bool,
    var_name: Option<String>,
    exported: bool,
    removal: Option<DeclRemoval>,
}

struct SxSite<'a> {
    attr_span: Span,
    expr: &'a Expression<'a>,
    local: String,
    bailed: Cell<bool>,
}

enum FlatArg<'a> {
    Expr(&'a Expression<'a>),
    Bad,
}

struct Classified {
    args: Vec<MergeArg>,
    tests: Vec<Span>,
}

/// One inlined static atom; props() hoists the ones it cannot merge away.
struct AtomStaticSite {
    span: Span,
    node_id: oxc_syntax::node::NodeId,
    edit_idx: usize,
    plan_idx: Option<usize>,
    compiled: Arc<JsObjectMap>,
    hoisted: bool,
}

/// (nearest package.json (name, dir) above the file, cwd's package name).
type DebugPaths = (Option<(String, String)>, Option<String>);

struct Shared<'a, 'env> {
    source: &'env str,
    state: &'env mut CompileState<'a>,
    fs: &'env dyn FsProvider,
    edits: Vec<Option<Edit>>,
    dead_spans: Vec<Span>,
    /// Sub-spans of dead spans spliced verbatim into the output (dynamic-fn
    /// expressions); the exit identifier pass still sees these upstream.
    live_spans: Vec<Span>,
    replaced_values: BTreeMap<u32, EvalValue>,
    create_sites: Vec<CreateSite>,
    sx_sites: Vec<SxSite<'a>>,
    style_map: BTreeMap<String, Arc<JsObjectMap>>,
    tuples: Vec<StyleVarToKeep>,
    export_locals: BTreeSet<String>,
    import_decls: Vec<(String, u32)>,
    debug_paths: Option<DebugPaths>,
    /// Generated sx runtime locals in generation order (babel state.stylexImport).
    sx_locals: Vec<String>,
    /// Generated prologue statements as (source offset of the site that made
    /// them, the unshifted group); babel's traversal order is source order.
    prologue_groups: Vec<(u32, Vec<PrologueStmt>)>,
    prologue_edit: Option<usize>,
    /// The `var _inject2 = _inject;` alias name — the callee of every inject
    /// call, memoized like upstream's injectImportInserted.
    inject_alias: Option<String>,
    /// Inject-call edit slots; DCE must not swallow them with their anchor.
    inject_edits: BTreeSet<usize>,
    /// Program-body offset where a generated import lands (after any hashbang
    /// and directive prologue, babel unshift-into-body semantics).
    import_insert_offset: u32,
    /// Uids handed out this compile (babel program.uids): later generateUid
    /// calls must skip them.
    generated_uids: BTreeSet<String>,
    /// Newline index over `source`, built on first debug-data lookup.
    line_index: Option<crate::state::LineIndex>,
    /// Static atoms already inlined, in traversal order; props() may hoist them.
    atom_static_sites: Vec<AtomStaticSite>,
    /// Callee spans of compiled dynamic atoms (`x.color` in `x.color(v)`).
    atom_dynamic_callees: BTreeSet<Span>,
    /// AST-backend twin of `edits` (seq = edit slot index at record time).
    plan: AstPlan,
    /// False on the splice path: the plan would be dropped unread, and its
    /// recording clones compiled maps and object values per site.
    build_plan: bool,
    /// Shared by every props()/attrs() evaluation: its inputs (import table,
    /// env) are fixed for the compile.
    props_registry: Option<FunctionRegistry>,
    error: Option<StylexError>,
}

pub fn transform_module<'a>(
    program: &'a Program<'a>,
    source: &str,
    state: &mut CompileState<'a>,
    fs: &dyn FsProvider,
    build_plan: bool,
    want_map: bool,
) -> Result<TransformOutput, StylexError> {
    let mut shared = Shared {
        source,
        state,
        fs,
        // Slot 0 is reserved for the generated prologue block: unshifted
        // imports print before same-anchor pass-A hoists (babel body order).
        edits: vec![None],
        dead_spans: Vec::new(),
        live_spans: Vec::new(),
        replaced_values: BTreeMap::new(),
        create_sites: Vec::new(),
        sx_sites: Vec::new(),
        style_map: BTreeMap::new(),
        tuples: Vec::new(),
        export_locals: collect_export_locals(program),
        import_decls: collect_import_decls(program),
        debug_paths: None,
        sx_locals: Vec::new(),
        prologue_groups: Vec::new(),
        prologue_edit: Some(0),
        inject_alias: None,
        inject_edits: BTreeSet::new(),
        import_insert_offset: import_insert_offset(program),
        generated_uids: BTreeSet::new(),
        line_index: None,
        atom_static_sites: Vec::new(),
        atom_dynamic_callees: BTreeSet::new(),
        plan: AstPlan::default(),
        build_plan,
        props_registry: None,
        error: None,
    };
    let index = pass_a_index(shared.state, source);
    {
        let mut pass = PassA {
            s: &mut shared,
            index,
        };
        walk_program(&mut pass, program);
    }
    if let Some(error) = shared.error.take() {
        return Err(error);
    }
    let has_atoms = !shared.state.imports.atom_imports.is_empty()
        || !shared.state.imports.atom_binding_locals.is_empty();
    if has_atoms {
        let index = pass_b1_index(shared.state, &shared.style_map);
        {
            let mut pass = PassB1 {
                s: &mut shared,
                index,
            };
            walk_program(&mut pass, program);
        }
        {
            let mut pass = PassAtoms {
                s: &mut shared,
                callee_spans: BTreeSet::new(),
            };
            walk_program(&mut pass, program);
        }
        if let Some(error) = shared.error.take() {
            return Err(error);
        }
        let index = pass_b2_index(shared.state, &shared.sx_sites);
        let mut pass = PassB2 {
            s: &mut shared,
            index,
        };
        walk_program(&mut pass, program);
    } else {
        // The atoms pass is the only writer of dead spans between B1 and B2;
        // without it the two Program.exit walks fuse into one.
        let mut index = pass_b1_index(shared.state, &shared.style_map);
        index
            .starts
            .extend(pass_b2_index(shared.state, &shared.sx_sites).starts);
        index.starts.sort_unstable();
        let mut pass = PassB12 {
            s: &mut shared,
            index,
            b1_skip: None,
            b2_skip: None,
        };
        walk_program(&mut pass, program);
    }
    if let Some(error) = shared.error.take() {
        return Err(error);
    }
    shared.finalize_sx_bails();
    shared.apply_dce();
    // Pre-DCE values by construction: apply_dce reads sites, never mutates
    // them, so the compiled maps move out instead of cloning.
    let create_objects: Vec<(Option<String>, Arc<JsObjectMap>)> = shared
        .create_sites
        .drain(..)
        .map(|site| (site.var_name, site.compiled))
        .collect();
    // Upstream rewrites in Program.exit, by which time the compensation
    // imports are in the tree: both copies of a source get the new text.
    let rewrites = shared.rewrite_import_sources(program);
    shared.insert_treeshake_imports(&rewrites);
    if shared.build_plan && !shared.prologue_groups.is_empty() {
        // Empty span at the anchor: no sourcemap mapping, and comments that
        // precede the anchor keep printing before the generated imports.
        let offset = shared.import_insert_offset;
        let stmts = shared.prologue_statements();
        shared.plan.inserts.push(InsertOp {
            anchor: offset,
            seq: 0,
            span: Span::new(offset, offset),
            stmt: SynthStmt::Prologue(stmts),
        });
    }
    let plan = std::mem::take(&mut shared.plan);
    let edits: Vec<Edit> = shared.edits.into_iter().flatten().collect();
    let modified = !edits.is_empty();
    // The AST backend emits from the mutated program, so splicing a whole
    // second copy of the source here would only be dropped by the caller.
    let mut splice_map = want_map.then(SpliceMap::default);
    let code = if shared.build_plan {
        String::new()
    } else {
        let _t = crate::timings::start(crate::timings::Stage::PrintSplice);
        apply_edits_tracked(source, &edits, splice_map.as_mut())
    };
    Ok(TransformOutput {
        code,
        modified,
        splice_map,
        create_objects,
        plan,
    })
}

fn collect_export_locals(program: &Program<'_>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for statement in &program.body {
        let Statement::ExportNamedDeclaration(export) = statement else {
            continue;
        };
        for specifier in &export.specifiers {
            if specifier.export_kind.is_type() {
                continue;
            }
            // parity: isVariableNamedExported — aliased exports do not count.
            let local = match &specifier.local {
                ModuleExportName::IdentifierReference(id) => id.name.as_str(),
                ModuleExportName::IdentifierName(id) => id.name.as_str(),
                ModuleExportName::StringLiteral(_) => continue,
            };
            let exported = match &specifier.exported {
                ModuleExportName::IdentifierReference(id) => id.name.as_str(),
                ModuleExportName::IdentifierName(id) => id.name.as_str(),
                ModuleExportName::StringLiteral(_) => continue,
            };
            if local == exported {
                out.insert(local.to_string());
            }
        }
    }
    out
}

fn import_insert_offset(program: &Program<'_>) -> u32 {
    if let Some(statement) = program.body.first() {
        return statement.span().start;
    }
    program
        .directives
        .last()
        .map(|d| d.span.end)
        .or_else(|| program.hashbang.as_ref().map(|h| h.span.end))
        .unwrap_or(0)
}

fn collect_import_decls(program: &Program<'_>) -> Vec<(String, u32)> {
    let mut out: Vec<(String, u32)> = Vec::new();
    for statement in &program.body {
        if let Statement::ImportDeclaration(decl) = statement
            && !out.iter().any(|(s, _)| s == decl.source.value.as_str())
        {
            out.push((decl.source.value.to_string(), decl.span.start));
        }
    }
    out
}

// parity: helper-module-imports stamps `_blockHoist = 3` on the inject import;
// babel's block-hoist plugin then stable-sorts the body by it, descending.
fn block_hoist(stmt: &PrologueStmt) -> u8 {
    match stmt {
        PrologueStmt::InjectImport { .. } => 3,
        _ => 0,
    }
}

fn prologue_text(stmt: &PrologueStmt) -> String {
    match stmt {
        PrologueStmt::NamespaceImport { local, source } => {
            format!("import * as {local} from {};\n", js_string_literal(source))
        }
        PrologueStmt::InjectImport {
            local,
            source,
            named: Some(imported),
        } => format!(
            "import {{ {imported} as {local} }} from {};\n",
            js_string_literal(source)
        ),
        PrologueStmt::InjectImport { local, source, .. } => {
            format!("import {local} from {};\n", js_string_literal(source))
        }
        PrologueStmt::InjectAlias { name, local } => format!("var {name} = {local};\n"),
    }
}

fn print_parens(map: &JsObjectMap) -> String {
    let mut out = String::with_capacity(estimate_object_len(map) + 2);
    out.push('(');
    write_object(map, &mut out);
    out.push(')');
    out
}

/// `print_parens` with dynamic (fns) namespace values printed as arrow text.
fn print_create_parens(map: &JsObjectMap, dynamic: &[(String, String)]) -> String {
    if dynamic.is_empty() {
        return print_parens(map);
    }
    let arrows: usize = dynamic.iter().map(|(_, arrow)| arrow.len()).sum();
    let mut out = String::with_capacity(estimate_object_len(map) + arrows + 2);
    out.push_str("({");
    for (i, (key, value)) in map.entries().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if is_identifier_key(key) {
            out.push_str(key);
        } else {
            crate::transform::js_out::write_js_string_literal(key, &mut out);
        }
        out.push_str(": ");
        match dynamic.iter().find(|(name, _)| name == key) {
            // parity: the fns rewrite only fires on object-shaped values.
            Some((_, arrow)) if matches!(value, EvalValue::Obj(_)) => out.push_str(arrow),
            _ => write_value(value, &mut out),
        }
    }
    out.push_str("})");
    out
}

fn jsx_attrs_text(map: &JsObjectMap) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in map.entries() {
        // parity: non-string AST values stringify as "[object Object]".
        let text = match value {
            EvalValue::Str(s) => s.replace('"', "&quot;"),
            _ => "[object Object]".to_string(),
        };
        parts.push(format!("{key}=\"{text}\""));
    }
    parts.join(" ")
}

impl<'a, 'env> Shared<'a, 'env> {
    fn fail(&mut self, error: StylexError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    fn push_edit(&mut self, edit: Edit) -> usize {
        self.edits.push(Some(edit));
        self.edits.len() - 1
    }

    /// Plan mode reads only edit spans (DCE, take_edits_within), never text.
    fn splice_text(&self, print: impl FnOnce() -> String) -> String {
        if self.build_plan {
            String::new()
        } else {
            print()
        }
    }

    fn in_dead(&self, span: Span) -> bool {
        self.dead_spans
            .iter()
            .any(|d| span.start >= d.start && span.end <= d.end)
            && !self
                .live_spans
                .iter()
                .any(|l| span.start >= l.start && span.end <= l.end)
    }

    /// [`Self::in_dead`] ignoring live sub-spans: an edit there would overlap
    /// the enclosing replacement, so atoms stay out of replaced source.
    fn within_dead_span(&self, span: Span) -> bool {
        self.dead_spans
            .iter()
            .any(|d| span.start >= d.start && span.end <= d.end)
    }

    fn contains_live(&self, span: Span) -> bool {
        self.live_spans
            .iter()
            .any(|l| l.start >= span.start && l.end <= span.end)
    }

    fn take_edits_within(&mut self, span: Span) -> Vec<Edit> {
        let mut taken = Vec::new();
        // Statement-level inserts anchor at a span start they do not belong to.
        let anchored = &self.inject_edits;
        let prologue = self.prologue_edit;
        for (idx, slot) in self.edits.iter_mut().enumerate() {
            if anchored.contains(&idx) || prologue == Some(idx) {
                continue;
            }
            if let Some(edit) = slot
                && edit.start >= span.start
                && edit.end <= span.end
            {
                taken.push(edit.clone());
                *slot = None;
            }
        }
        taken
    }

    fn is_named_export(&self, ctx: &DeclCtx) -> bool {
        ctx.exported
            || (ctx.top_level
                && ctx
                    .name
                    .as_deref()
                    .is_some_and(|n| self.export_locals.contains(n)))
    }

    // parity: state-manager fileNameForHashing.
    fn file_name_for_hashing(&self) -> Option<String> {
        let filename = self.state.filename.as_deref()?;
        let resolution = self.state.options.unstable_module_resolution.as_ref()?;
        if !matches_file_suffix(&resolution.theme_file_extension, filename)
            && !matches_file_suffix(&resolution.consts_file_extension(), filename)
        {
            return None;
        }
        match resolution.kind {
            ModuleResolutionType::Haste => Some(node_basename(filename).to_string()),
            ModuleResolutionType::CommonJs => Some(canonical_file_path(
                self.fs,
                Path::new(filename),
                resolution.root_dir.as_deref(),
            )),
        }
    }

    fn debug_paths(&mut self) -> DebugPaths {
        if let Some(cached) = &self.debug_paths {
            return cached.clone();
        }
        let need = self.state.options.debug && self.state.options.enable_debug_data_prop;
        let value = match (&self.state.filename, need) {
            (Some(filename), true) => {
                let file_package = self
                    .fs
                    .nearest_package(Path::new(filename))
                    .map(|(name, dir)| (name, dir.to_string_lossy().into_owned()));
                let cwd_package_name = if file_package.is_some() {
                    self.fs
                        .nearest_package(Path::new(&self.state.cwd))
                        .map(|(name, _)| name)
                } else {
                    None
                };
                (file_package, cwd_package_name)
            }
            _ => (None, None),
        };
        self.debug_paths = Some(value.clone());
        value
    }

    fn record_override(&mut self, ctx: &DeclCtx, value: EvalValue) {
        if ctx.top_level
            && let Some(name) = &ctx.name
        {
            self.state.record_root_override(name, value);
        }
    }

    fn line_of(&mut self, offset: u32) -> u32 {
        self.line_index
            .get_or_insert_with(|| crate::state::LineIndex::build(self.source))
            .line_of(offset)
    }

    // ---- pass A transforms -------------------------------------------------

    fn transform_create(&mut self, call: &'a CallExpression<'a>, parent: ExprParent<'_>) {
        if matches!(parent, ExprParent::Statement) {
            return self.fail(StylexError::unbound_call_value("create"));
        }
        let object = match validate_create_arg(call) {
            Ok(object) => object,
            Err(error) => return self.fail(error),
        };
        let arg = {
            let mut evaluator = Evaluator::for_create(&mut *self.state, self.fs, &RealCallables);
            evaluator.allow_dynamic_namespaces();
            match evaluate_stylex_create_arg(&mut evaluator, object) {
                Ok(arg) => arg,
                Err(error) => return self.fail(error),
            }
        };
        // Only the debug data-prop (`$$css: "file:line"`) reads these; prod
        // compiles skip the per-key line lookups entirely.
        let need_lines = self.state.options.debug && self.state.options.enable_debug_data_prop;
        let mut namespace_lines = BTreeMap::new();
        if need_lines {
            for (key, offset) in &arg.key_spans {
                let line = self.line_of(*offset);
                namespace_lines.insert(key.clone(), line);
            }
        }
        let decl = match parent {
            ExprParent::Declarator(ctx) => Some(ctx.clone()),
            _ => None,
        };
        let var_name = decl.as_ref().and_then(|c| c.name.clone());
        let (file_package, cwd_package_name) = self.debug_paths();
        let ctx = CreateContext {
            options: self.state.options,
            filename: self.state.filename.clone(),
            cwd: self.state.cwd.clone(),
            var_name: var_name.clone(),
            namespace_lines: Some(namespace_lines),
            file_package,
            cwd_package_name,
        };
        let out = match compile_namespaces(&arg.namespaces, &ctx, !arg.fns.is_empty()) {
            Ok(out) => out,
            Err(error) => return self.fail(error),
        };
        // parity: injectedStyles = {...otherInjected, ...createRules, ...inherit}
        // — one map, also the rule-lookup the fns rewrite scans.
        let mut injected = std::mem::take(&mut self.state.other_injected_rules);
        injected.extend(out.rules);
        injected.extend(inherit_rules(&arg.fns));
        let call_node = call.node_id.get();
        let stmt_start = self.state.program_statement_start(call_node);
        let mut dynamic: Vec<(String, String)> = Vec::new();
        let mut dynamic_entries: Vec<DynamicEntry> = Vec::new();
        let mut hoist_edit_idxs: Vec<usize> = Vec::new();
        for (ns, fn_def) in &arg.fns {
            let Some(EvalValue::Obj(compiled_ns)) = out.compiled.get(ns) else {
                continue;
            };
            let empty = Vec::new();
            let class_paths = out
                .class_paths
                .iter()
                .find(|(name, _)| name == ns)
                .map(|(_, paths)| paths)
                .unwrap_or(&empty);
            let dc = compile_dynamic_namespace(
                compiled_ns,
                class_paths,
                fn_def,
                &injected,
                self.state.options,
            );
            for var in &dc.inline_vars {
                self.live_spans.push(var.expr);
            }
            // parity: stylex-create.js:471 hoistExpression — the static chunk
            // becomes a module const so call results share object identity.
            let static_ident = if dc.static_props.is_empty() {
                None
            } else {
                let uid = self.generate_uid("temp", call_node);
                let text =
                    self.splice_text(|| format!("const {uid} = {};\n", print_static_chunk(&dc)));
                let idx = self.push_edit(Edit::insert(stmt_start, text));
                hoist_edit_idxs.push(idx);
                if self.build_plan {
                    self.plan.inserts.push(InsertOp {
                        anchor: stmt_start,
                        seq: idx,
                        span: call.span,
                        stmt: SynthStmt::ConstDecl {
                            name: uid.clone(),
                            value: HoistValue::StaticChunk(dc.clone()),
                        },
                    });
                }
                Some(uid)
            };
            if !self.build_plan {
                dynamic.push((
                    ns.clone(),
                    print_dynamic_arrow(&dc, self.source, static_ident.as_deref()),
                ));
            }
            dynamic_entries.push(DynamicEntry {
                namespace: ns.clone(),
                compiled: dc,
                static_ident,
            });
        }
        self.register_rules(injected, call);

        let style_var = decl
            .as_ref()
            .is_some_and(|c| c.top_level && c.name.is_some());
        if style_var {
            let name = var_name.clone().expect("style vars have names");
            self.style_map.insert(name, out.compiled.clone());
            if let Some(c) = &decl {
                self.record_override(c, EvalValue::Obj(out.compiled.clone()));
            }
        }
        // parity: pathReplaceHoisted — non-program-level create results hoist
        // into a collision-free module const so object identity is shared.
        let (edit_idx, plan_idx) = if self.state.is_program_level(call_node) {
            let plan_idx = self.build_plan.then(|| {
                let plan_idx = self.plan.replace_exprs.len();
                self.plan.replace_exprs.push((
                    call.span,
                    SynthExpr::CreateObject {
                        map: out.compiled.clone(),
                        dynamic: dynamic_entries.clone(),
                    },
                ));
                plan_idx
            });
            let text = self.splice_text(|| print_create_parens(&out.compiled, &dynamic));
            let edit_idx = self.push_edit(Edit::replace(call.span, text));
            (edit_idx, plan_idx)
        } else {
            let uid = self.generate_uid("styles", call_node);
            let text = self.splice_text(|| {
                format!(
                    "const {uid} = {};\n",
                    print_create_parens(&out.compiled, &dynamic)
                )
            });
            let hoist_idx = self.push_edit(Edit::insert(stmt_start, text));
            if self.build_plan {
                self.plan.inserts.push(InsertOp {
                    anchor: stmt_start,
                    seq: hoist_idx,
                    span: call.span,
                    stmt: SynthStmt::ConstDecl {
                        name: uid.clone(),
                        value: HoistValue::CreateObject {
                            map: out.compiled.clone(),
                            dynamic: dynamic_entries.clone(),
                        },
                    },
                });
                self.plan
                    .replace_exprs
                    .push((call.span, SynthExpr::Ident(uid.clone())));
            }
            (self.push_edit(Edit::replace(call.span, uid)), None)
        };
        self.dead_spans.push(call.span);
        self.create_sites.push(CreateSite {
            span: call.span,
            edit_idx,
            plan_idx,
            hoist_edit_idxs,
            compiled: out.compiled,
            dynamic,
            dynamic_entries,
            style_var,
            var_name,
            exported: decl.as_ref().is_some_and(|c| c.exported),
            removal: decl.and_then(|c| c.removal),
        });
    }

    fn transform_keyframes(&mut self, call: &'a CallExpression<'a>, ctx: &DeclCtx) {
        if call.arguments.len() != 1 {
            return self.fail(StylexError::illegal_argument_length("keyframes", 1));
        }
        let arg_expr = call.arguments[0].as_expression();
        let Some(arg_expr) = arg_expr else {
            return self.fail(StylexError::non_style_object("keyframes"));
        };
        if !matches!(unwrap_parens(arg_expr), Expression::ObjectExpression(_)) {
            return self.fail(StylexError::non_style_object("keyframes"));
        }
        let registry =
            FunctionRegistry::for_keyframes(&self.state.imports, &self.state.options.env);
        let outcome = {
            let mut evaluator = Evaluator::with_registry(
                &mut *self.state,
                self.fs,
                &RealCallables,
                registry,
                false,
            );
            evaluator.eval(arg_expr)
        };
        let value = match outcome {
            Ok(EvalOutcome::Value(v)) => into_eval_value(v),
            Ok(EvalOutcome::NonStatic(_)) => {
                return self.fail(StylexError::non_static_value("keyframes"));
            }
            Err(error) => return self.fail(error),
        };
        let (name, rule) = match keyframes(&value, self.state.options) {
            Ok(out) => out,
            Err(error) => return self.fail(error),
        };
        self.register_rules(vec![rule], call);
        let text = self.splice_text(|| js_string_literal(&name));
        self.push_edit(Edit::replace(call.span, text));
        self.plan
            .replace_exprs
            .push((call.span, SynthExpr::Str(name.clone())));
        self.dead_spans.push(call.span);
        self.replaced_values
            .insert(call.span.start, EvalValue::Str(name.clone()));
        self.record_override(ctx, EvalValue::Str(name));
    }

    fn transform_default_marker(&mut self, call: &'a CallExpression<'a>, parent: ExprParent<'_>) {
        if !call.arguments.is_empty() {
            return self.fail(StylexError::illegal_argument_length("defaultMarker", 0));
        }
        let object = Arc::new(default_marker_object(self.state.options));
        let text = self.splice_text(|| print_parens(&object));
        self.push_edit(Edit::replace(call.span, text));
        if self.build_plan {
            self.plan.replace_exprs.push((
                call.span,
                SynthExpr::ParenValue(EvalValue::Obj(Arc::clone(&object))),
            ));
        }
        self.dead_spans.push(call.span);
        self.replaced_values
            .insert(call.span.start, EvalValue::Obj(Arc::clone(&object)));
        if let ExprParent::Declarator(ctx) = parent {
            self.record_override(ctx, EvalValue::Obj(object));
        }
    }

    fn transform_define_marker(&mut self, call: &'a CallExpression<'a>, parent: ExprParent<'_>) {
        let ExprParent::Declarator(ctx) = parent else {
            return self.fail(StylexError::unbound_call_value("defineMarker"));
        };
        let ctx = ctx.clone();
        let Some(name) = ctx.name.clone() else {
            return self.fail(StylexError::unbound_call_value("defineMarker"));
        };
        if !self.is_named_export(&ctx) {
            return self.fail(StylexError::non_export_named_declaration("defineMarker"));
        }
        if !call.arguments.is_empty() {
            return self.fail(StylexError::illegal_argument_length("defineMarker", 0));
        }
        let Some(file) = self.file_name_for_hashing() else {
            return self.fail(StylexError::cannot_generate_hash("defineMarker"));
        };
        let object = Arc::new(define_marker_object(&file, &name, self.state.options));
        let text = self.splice_text(|| print_parens(&object));
        self.push_edit(Edit::replace(call.span, text));
        if self.build_plan {
            self.plan.replace_exprs.push((
                call.span,
                SynthExpr::ParenValue(EvalValue::Obj(Arc::clone(&object))),
            ));
        }
        self.dead_spans.push(call.span);
        self.replaced_values
            .insert(call.span.start, EvalValue::Obj(Arc::clone(&object)));
        self.record_override(&ctx, EvalValue::Obj(object));
    }

    fn transform_define_consts(&mut self, call: &'a CallExpression<'a>, parent: ExprParent<'_>) {
        let ExprParent::Declarator(ctx) = parent else {
            return self.fail(StylexError::unbound_call_value("defineConsts"));
        };
        let ctx = ctx.clone();
        let Some(name) = ctx.name.clone() else {
            return self.fail(StylexError::unbound_call_value("defineConsts"));
        };
        if !self.is_named_export(&ctx) {
            return self.fail(StylexError::non_export_named_declaration("defineConsts"));
        }
        if call.arguments.len() != 1 {
            return self.fail(StylexError::illegal_argument_length("defineConsts", 1));
        }
        let Some(arg_expr) = call.arguments[0].as_expression() else {
            return self.fail(StylexError::non_static_value("defineConsts"));
        };
        let registry = FunctionRegistry::for_consts(&self.state.imports, &self.state.options.env);
        let outcome = {
            let mut evaluator =
                Evaluator::with_registry(&mut *self.state, self.fs, &RealCallables, registry, true);
            evaluator.eval(arg_expr)
        };
        let value = match outcome {
            Ok(EvalOutcome::Value(v)) => into_eval_value(v),
            Ok(EvalOutcome::NonStatic(_)) => {
                return self.fail(StylexError::non_static_value("defineConsts"));
            }
            Err(error) => return self.fail(error),
        };
        if !matches!(value, EvalValue::Obj(_) | EvalValue::Arr(_)) {
            return self.fail(StylexError::non_style_object("defineConsts"));
        }
        let Some(file) = self.file_name_for_hashing() else {
            return self.fail(StylexError::cannot_generate_hash("defineConsts"));
        };
        let entries: Vec<EvalValue> = match &value {
            EvalValue::Obj(map) => map.entries().map(|(_, v)| v.clone()).collect(),
            EvalValue::Arr(items) => items.clone(),
            _ => unreachable!("checked above"),
        };
        let mut out = match define_consts(&value, &file, &name, self.state.options) {
            Ok(out) => out,
            Err(error) => return self.fail(error),
        };
        // JSON cannot carry the non-finite constVal babel emits; re-tag it.
        for (rule, source_value) in out.rules.iter_mut().zip(entries.iter()) {
            if let EvalValue::Num(n) = source_value
                && !n.is_finite()
            {
                rule.const_val = non_finite_to_tag(*n).map(Box::new);
            }
        }
        self.register_rules(out.rules, call);
        let text = self.splice_text(|| print_parens(&out.js_output));
        self.push_edit(Edit::replace(call.span, text));
        if self.build_plan {
            self.plan.replace_exprs.push((
                call.span,
                SynthExpr::ParenValue(EvalValue::Obj(Arc::new(out.js_output.clone()))),
            ));
        }
        self.dead_spans.push(call.span);
        self.replaced_values.insert(
            call.span.start,
            EvalValue::Obj(Arc::new(out.js_output.clone())),
        );
        self.record_override(&ctx, EvalValue::Obj(Arc::new(out.js_output)));
    }

    /// Replaces a theming-call span and records both value seams (props()
    /// nullable parsing + babel binding re-resolution).
    fn finish_replacement(&mut self, ctx: &DeclCtx, span: Span, value: EvalValue, text: String) {
        self.push_edit(Edit::replace(span, text));
        if self.build_plan {
            // The splice text is js_string_literal for strings, print_parens else.
            let synth = match &value {
                EvalValue::Str(name) => SynthExpr::Str(name.clone()),
                other => SynthExpr::ParenValue(other.clone()),
            };
            self.plan.replace_exprs.push((span, synth));
        }
        self.dead_spans.push(span);
        self.replaced_values.insert(span.start, value.clone());
        self.record_override(ctx, value);
    }

    fn drain_injected_then(&mut self, rules: Vec<StylexRule>, call: &'a CallExpression<'a>) {
        let mut injected = std::mem::take(&mut self.state.other_injected_rules);
        injected.extend(rules);
        self.register_rules(injected, call);
    }

    fn register_rules(&mut self, rules: Vec<StylexRule>, call: &'a CallExpression<'a>) {
        self.register_rules_at(rules, call.node_id.get(), call.span.start);
    }

    /// parity: state-manager.js registerStyles — metadata plus, under
    /// runtimeInjection, one inject call per rule before the anchor statement.
    fn register_rules_at(
        &mut self,
        rules: Vec<StylexRule>,
        node: oxc_syntax::node::NodeId,
        site_offset: u32,
    ) {
        if self.state.options.runtime_injection.is_some() && !rules.is_empty() {
            let callee = self.inject_callee(node, site_offset);
            let anchor = self.state.program_statement_start(node);
            for rule in &rules {
                let text = self.splice_text(|| inject_call_text(&callee, rule));
                let idx = self.push_edit(Edit::insert(anchor, text));
                self.inject_edits.insert(idx);
                if self.build_plan {
                    self.plan.inserts.push(InsertOp {
                        anchor,
                        seq: idx,
                        span: Span::new(anchor, anchor),
                        stmt: SynthStmt::InjectCall {
                            callee: callee.clone(),
                            rule: Box::new(rule.clone()),
                        },
                    });
                }
            }
        }
        self.state.rules.extend(rules);
    }

    fn transform_position_try(&mut self, call: &'a CallExpression<'a>, ctx: &DeclCtx) {
        if call.arguments.len() != 1 {
            return self.fail(StylexError::illegal_argument_length("positionTry", 1));
        }
        let Some(arg_expr) = call.arguments[0].as_expression() else {
            return self.fail(StylexError::non_static_value("positionTry"));
        };
        let registry =
            FunctionRegistry::for_keyframes(&self.state.imports, &self.state.options.env);
        let outcome = {
            let mut evaluator = Evaluator::with_registry(
                &mut *self.state,
                self.fs,
                &RealCallables,
                registry,
                false,
            );
            evaluator.eval(arg_expr)
        };
        let value = match outcome {
            Ok(EvalOutcome::Value(v)) => into_eval_value(v),
            Ok(EvalOutcome::NonStatic(_)) => {
                return self.fail(StylexError::non_static_value("positionTry"));
            }
            Err(error) => return self.fail(error),
        };
        let (name, rule) = match position_try(&value, self.state.options) {
            Ok(out) => out,
            Err(error) => return self.fail(error),
        };
        self.register_rules(vec![rule], call);
        let text = self.splice_text(|| js_string_literal(&name));
        self.finish_replacement(ctx, call.span, EvalValue::Str(name), text);
    }

    fn transform_view_transition(&mut self, call: &'a CallExpression<'a>, ctx: &DeclCtx) {
        if call.arguments.len() != 1 {
            return self.fail(StylexError::illegal_argument_length(
                "viewTransitionClass",
                1,
            ));
        }
        let Some(arg_expr) = call.arguments[0].as_expression() else {
            return self.fail(StylexError::non_static_value("viewTransitionClass"));
        };
        let registry =
            FunctionRegistry::for_view_transition(&self.state.imports, &self.state.options.env);
        let outcome = {
            let mut evaluator = Evaluator::with_registry(
                &mut *self.state,
                self.fs,
                &VarsCallables,
                registry,
                false,
            );
            evaluator.eval(arg_expr)
        };
        let value = match outcome {
            Ok(EvalOutcome::Value(v)) => into_eval_value(v),
            Ok(EvalOutcome::NonStatic(_)) => {
                return self.fail(StylexError::non_static_value("viewTransitionClass"));
            }
            Err(error) => return self.fail(error),
        };
        let (name, rule) = match view_transition_class(&value, self.state.options) {
            Ok(out) => out,
            Err(error) => return self.fail(error),
        };
        self.drain_injected_then(vec![rule], call);
        let text = self.splice_text(|| js_string_literal(&name));
        self.finish_replacement(ctx, call.span, EvalValue::Str(name), text);
    }

    fn transform_define_vars(&mut self, call: &'a CallExpression<'a>, parent: ExprParent<'_>) {
        let ExprParent::Declarator(ctx) = parent else {
            return self.fail(StylexError::unbound_call_value("defineVars"));
        };
        let ctx = ctx.clone();
        let Some(name) = ctx.name.clone() else {
            return self.fail(StylexError::unbound_call_value("defineVars"));
        };
        if !self.is_named_export(&ctx) {
            return self.fail(StylexError::non_export_named_declaration("defineVars"));
        }
        if call.arguments.len() != 1 {
            return self.fail(StylexError::illegal_argument_length("defineVars", 1));
        }
        let Some(canonical) = self.file_name_for_hashing() else {
            return self.fail(StylexError::cannot_generate_hash("defineVars"));
        };
        let Some(arg_expr) = call.arguments[0].as_expression() else {
            return self.fail(StylexError::non_static_value("defineVars"));
        };
        let registry = vars_registry(&self.state.imports, &self.state.options.env);
        let output = {
            let mut evaluator = Evaluator::with_registry(
                &mut *self.state,
                self.fs,
                &VarsCallables,
                registry,
                false,
            );
            let value = match evaluator.eval(arg_expr) {
                Ok(EvalOutcome::Value(v)) => v,
                Ok(EvalOutcome::NonStatic(_)) => {
                    return self.fail(StylexError::non_static_value("defineVars"));
                }
                Err(error) => return self.fail(error),
            };
            match define_vars(&mut evaluator, &value, &canonical, &name) {
                Ok(out) => out,
                Err(error) => return self.fail(error),
            }
        };
        self.drain_injected_then(output.rules, call);
        let text = self.splice_text(|| print_parens(&output.js_output));
        self.finish_replacement(
            &ctx,
            call.span,
            EvalValue::Obj(Arc::new(output.js_output)),
            text,
        );
    }

    fn transform_create_theme(&mut self, call: &'a CallExpression<'a>, parent: ExprParent<'_>) {
        let ExprParent::Declarator(ctx) = parent else {
            return self.fail(StylexError::unbound_call_value("createTheme"));
        };
        let ctx = ctx.clone();
        let Some(name) = ctx.name.clone() else {
            return self.fail(StylexError::unbound_call_value("createTheme"));
        };
        if call.arguments.len() != 2 {
            return self.fail(StylexError::illegal_argument_length("createTheme", 2));
        }
        let Some((theme_vars, overrides)) = self.eval_theme_args(call, "createTheme") else {
            return;
        };
        let output = match create_theme(&theme_vars, &overrides, self.state.options) {
            Ok(out) => out,
            Err(error) => return self.fail(error),
        };
        let js_output = apply_theme_dev_naming(
            output.js_output,
            self.state.filename.as_deref(),
            &name,
            self.state.options,
        );
        self.drain_injected_then(output.rules, call);
        let text = self.splice_text(|| print_parens(&js_output));
        self.finish_replacement(&ctx, call.span, EvalValue::Obj(Arc::new(js_output)), text);
    }

    /// The two createTheme argument evaluations: first with no FunctionConfig,
    /// second with the vars registry. `None` = already failed.
    fn eval_theme_args(
        &mut self,
        call: &'a CallExpression<'a>,
        api: &str,
    ) -> Option<(JsValue, JsValue)> {
        let (Some(arg0), Some(arg1)) = (
            call.arguments[0].as_expression(),
            call.arguments[1].as_expression(),
        ) else {
            self.fail(StylexError::non_static_value(api));
            return None;
        };
        let theme_vars = {
            let mut evaluator = Evaluator::with_registry(
                &mut *self.state,
                self.fs,
                &VarsCallables,
                FunctionRegistry::default(),
                false,
            );
            match evaluator.eval(arg0) {
                Ok(EvalOutcome::Value(v)) => v,
                Ok(EvalOutcome::NonStatic(_)) => {
                    self.fail(StylexError::non_static_value(api));
                    return None;
                }
                Err(error) => {
                    self.fail(error);
                    return None;
                }
            }
        };
        let registry = vars_registry(&self.state.imports, &self.state.options.env);
        let overrides = {
            let mut evaluator = Evaluator::with_registry(
                &mut *self.state,
                self.fs,
                &VarsCallables,
                registry,
                false,
            );
            match evaluator.eval(arg1) {
                Ok(EvalOutcome::Value(v)) => v,
                Ok(EvalOutcome::NonStatic(_)) => {
                    self.fail(StylexError::non_static_value(api));
                    return None;
                }
                Err(error) => {
                    self.fail(error);
                    return None;
                }
            }
        };
        Some((theme_vars, overrides))
    }

    fn transform_define_vars_nested(
        &mut self,
        call: &'a CallExpression<'a>,
        parent: ExprParent<'_>,
    ) {
        const API: &str = "unstable_defineVarsNested";
        let ExprParent::Declarator(ctx) = parent else {
            return self.fail(StylexError::unbound_call_value(API));
        };
        let ctx = ctx.clone();
        let Some(name) = ctx.name.clone() else {
            return self.fail(StylexError::unbound_call_value(API));
        };
        if !self.is_named_export(&ctx) {
            return self.fail(StylexError::non_export_named_declaration(API));
        }
        if call.arguments.len() != 1 {
            return self.fail(StylexError::illegal_argument_length(API, 1));
        }
        let Some(arg_expr) = call.arguments[0].as_expression() else {
            return self.fail(StylexError::non_static_value(API));
        };
        let registry = vars_registry(&self.state.imports, &self.state.options.env);
        let outcome = {
            let mut evaluator = Evaluator::with_registry(
                &mut *self.state,
                self.fs,
                &VarsCallables,
                registry,
                false,
            );
            evaluator.eval(arg_expr)
        };
        let value = match outcome {
            Ok(EvalOutcome::Value(v)) => v,
            Ok(EvalOutcome::NonStatic(_)) => {
                return self.fail(StylexError::non_static_value(API));
            }
            Err(error) => return self.fail(error),
        };
        if !matches!(value, JsValue::Obj(_) | JsValue::Arr(_) | JsValue::Proxy(_)) {
            return self.fail(StylexError::non_style_object(API));
        }
        let Some(canonical) = self.file_name_for_hashing() else {
            return self.fail(StylexError::cannot_generate_hash(API));
        };
        let flat = match flatten_nested_vars_config(&value) {
            Ok(flat) => flat,
            Err(error) => return self.fail(error),
        };
        let entries: Vec<(String, JsValue)> = flat
            .entries()
            .map(|(k, v)| (k.to_string(), from_eval_value(v)))
            .collect();
        let output = match define_vars_core(&entries, &canonical, &name, self.state.options) {
            Ok(out) => out,
            Err(error) => return self.fail(error),
        };
        let js_output = nest_define_vars_js_output(&output.js_output);
        self.drain_injected_then(output.rules, call);
        let text = self.splice_text(|| print_parens(&js_output));
        self.finish_replacement(&ctx, call.span, EvalValue::Obj(Arc::new(js_output)), text);
    }

    fn transform_define_consts_nested(
        &mut self,
        call: &'a CallExpression<'a>,
        parent: ExprParent<'_>,
    ) {
        const API: &str = "unstable_defineConstsNested";
        let ExprParent::Declarator(ctx) = parent else {
            return self.fail(StylexError::unbound_call_value(API));
        };
        let ctx = ctx.clone();
        let Some(name) = ctx.name.clone() else {
            return self.fail(StylexError::unbound_call_value(API));
        };
        if !self.is_named_export(&ctx) {
            return self.fail(StylexError::non_export_named_declaration(API));
        }
        if call.arguments.len() != 1 {
            return self.fail(StylexError::illegal_argument_length(API, 1));
        }
        let Some(arg_expr) = call.arguments[0].as_expression() else {
            return self.fail(StylexError::non_static_value(API));
        };
        let registry = FunctionRegistry::for_consts(&self.state.imports, &self.state.options.env);
        let outcome = {
            let mut evaluator =
                Evaluator::with_registry(&mut *self.state, self.fs, &RealCallables, registry, true);
            evaluator.eval(arg_expr)
        };
        let value = match outcome {
            Ok(EvalOutcome::Value(v)) => into_eval_value(v),
            Ok(EvalOutcome::NonStatic(_)) => {
                return self.fail(StylexError::non_static_value(API));
            }
            Err(error) => return self.fail(error),
        };
        if !matches!(value, EvalValue::Obj(_) | EvalValue::Arr(_)) {
            return self.fail(StylexError::non_style_object(API));
        }
        let Some(canonical) = self.file_name_for_hashing() else {
            return self.fail(StylexError::cannot_generate_hash(API));
        };
        let mut output = match define_consts_nested(&value, &canonical, &name, self.state.options) {
            Ok(out) => out,
            Err(error) => return self.fail(error),
        };
        // JSON cannot carry non-finite constVals; re-tag from the flat entries.
        if let Ok(flat) = flatten_nested_consts_config(&value) {
            for (rule, (_, source_value)) in output.rules.iter_mut().zip(flat.entries()) {
                if let EvalValue::Num(n) = source_value
                    && !n.is_finite()
                {
                    rule.const_val = non_finite_to_tag(*n).map(Box::new);
                }
            }
        }
        self.register_rules(output.rules, call);
        let text = self.splice_text(|| print_parens(&output.js_output));
        self.finish_replacement(
            &ctx,
            call.span,
            EvalValue::Obj(Arc::new(output.js_output)),
            text,
        );
    }

    fn transform_create_theme_nested(
        &mut self,
        call: &'a CallExpression<'a>,
        parent: ExprParent<'_>,
    ) {
        const API: &str = "unstable_createThemeNested";
        let ExprParent::Declarator(ctx) = parent else {
            return self.fail(StylexError::unbound_call_value(API));
        };
        let ctx = ctx.clone();
        let Some(name) = ctx.name.clone() else {
            return self.fail(StylexError::unbound_call_value(API));
        };
        if call.arguments.len() != 2 {
            return self.fail(StylexError::illegal_argument_length(API, 2));
        }
        let Some(arg0) = call.arguments[0].as_expression() else {
            return self.fail(StylexError::non_static_value(API));
        };
        let theme_vars = {
            let mut evaluator = Evaluator::with_registry(
                &mut *self.state,
                self.fs,
                &VarsCallables,
                FunctionRegistry::default(),
                false,
            );
            match evaluator.eval(arg0) {
                Ok(EvalOutcome::Value(v)) => v,
                Ok(EvalOutcome::NonStatic(_)) => {
                    return self.fail(StylexError::non_static_value(API));
                }
                Err(error) => return self.fail(error),
            }
        };
        // parity: the __varGroupHash__ check runs before the overrides evaluate.
        let var_group_hash = match &theme_vars {
            JsValue::Proxy(proxy) => proxy.var_group_hash().to_string(),
            JsValue::Obj(obj) => match obj.get("__varGroupHash__") {
                Some(JsValue::Str(s)) if !s.is_empty() => s.clone(),
                _ => return self.fail(StylexError::nested_theme_invalid_vars()),
            },
            JsValue::Null | JsValue::Undefined => {
                let kind = if matches!(theme_vars, JsValue::Null) {
                    "null"
                } else {
                    "undefined"
                };
                return self.fail(StylexError::new(
                    crate::errors::ErrorCode::NonStaticValue,
                    format!("Cannot read properties of {kind} (reading '__varGroupHash__')"),
                ));
            }
            _ => return self.fail(StylexError::nested_theme_invalid_vars()),
        };
        let Some(arg1) = call.arguments[1].as_expression() else {
            return self.fail(StylexError::non_static_value(API));
        };
        let registry = vars_registry(&self.state.imports, &self.state.options.env);
        let overrides = {
            let mut evaluator = Evaluator::with_registry(
                &mut *self.state,
                self.fs,
                &VarsCallables,
                registry,
                false,
            );
            match evaluator.eval(arg1) {
                Ok(EvalOutcome::Value(v)) => v,
                Ok(EvalOutcome::NonStatic(_)) => {
                    return self.fail(StylexError::non_static_value(API));
                }
                Err(error) => return self.fail(error),
            }
        };
        if !matches!(
            overrides,
            JsValue::Obj(_) | JsValue::Arr(_) | JsValue::Proxy(_)
        ) {
            return self.fail(StylexError::non_style_object(API));
        }
        let mut nested_refs = JsObjectMap::new();
        if let EvalValue::Obj(map) = to_eval_value(&theme_vars) {
            for (key, value) in map.entries() {
                if key != "__varGroupHash__" {
                    nested_refs.insert(key, value.clone());
                }
            }
        }
        let mut flat_theme_vars =
            match flatten_nested_string_config(&EvalValue::Obj(Arc::new(nested_refs))) {
                Ok(flat) => flat,
                Err(error) => return self.fail(error),
            };
        flat_theme_vars.insert("__varGroupHash__", EvalValue::Str(var_group_hash));
        let flat_overrides = match flatten_nested_overrides_config(&overrides) {
            Ok(flat) => flat,
            Err(error) => return self.fail(error),
        };
        let output = match create_theme(
            &into_js_value(EvalValue::Obj(Arc::new(flat_theme_vars))),
            &into_js_value(EvalValue::Obj(Arc::new(flat_overrides))),
            self.state.options,
        ) {
            Ok(out) => out,
            Err(error) => return self.fail(error),
        };
        let js_output = apply_theme_dev_naming(
            output.js_output,
            self.state.filename.as_deref(),
            &name,
            self.state.options,
        );
        self.drain_injected_then(output.rules, call);
        let text = self.splice_text(|| print_parens(&js_output));
        self.finish_replacement(&ctx, call.span, EvalValue::Obj(Arc::new(js_output)), text);
    }

    // ---- pass Atoms: @stylexjs/atoms -------------------------------------

    fn atom_site(&self, span: Span) -> Option<usize> {
        self.atom_static_sites.iter().position(|s| s.span == span)
    }

    fn atom_context(&self) -> CreateContext<'_> {
        CreateContext {
            options: self.state.options,
            filename: self.state.filename.clone(),
            cwd: self.state.cwd.clone(),
            var_name: None,
            namespace_lines: None,
            file_package: None,
            cwd_package_name: None,
        }
    }

    // parity: atoms babel-transform compileStaticStyle.
    fn transform_atom_static(&mut self, info: &MemberInfo<'a>, property: &str, value: &str) {
        let out = match compile_atom(property, value, &self.atom_context()) {
            Ok(out) => out,
            Err(error) => return self.fail(error),
        };
        self.register_rules_at(out.rules, info.node_id, info.span.start);
        let Some(EvalValue::Obj(compiled)) = out.compiled.get(INLINE_NAMESPACE).cloned() else {
            return;
        };
        // Upstream's replaceWith throws when the member is an assignment target.
        if info.expr.is_none() {
            return self.fail(StylexError::upstream_type_crash(
                "an atom in assignment-target position",
            ));
        }
        let plan_idx = self.build_plan.then(|| {
            let plan_idx = self.plan.replace_exprs.len();
            self.plan.replace_exprs.push((
                info.span,
                SynthExpr::ParenValue(EvalValue::Obj(compiled.clone())),
            ));
            plan_idx
        });
        let text = self.splice_text(|| print_parens(&compiled));
        let edit_idx = self.push_edit(Edit::replace(info.span, text));
        self.dead_spans.push(info.span);
        self.atom_static_sites.push(AtomStaticSite {
            span: info.span,
            node_id: info.node_id,
            edit_idx,
            plan_idx,
            compiled,
            hoisted: false,
        });
    }

    // parity: atoms babel-transform compileDynamicStyle. Only the callee is
    // rewritten: upstream rebuilds the call around the same argument node.
    fn transform_atom_dynamic(&mut self, call: &'a CallExpression<'a>, property: &str) {
        let var_name = format!("--x-{property}");
        let out = match compile_atom(property, &format!("var({var_name})"), &self.atom_context()) {
            Ok(out) => out,
            Err(error) => return self.fail(error),
        };
        let node = call.node_id.get();
        let mut rules = out.rules;
        rules.push(StylexRule {
            class_name: var_name.as_str().into(),
            ltr: format!("@property {var_name} {{ syntax: \"*\"; inherits: false;}}").into(),
            rtl: None,
            const_key: None,
            const_val: None,
            priority: 0.0,
        });
        self.register_rules_at(rules, node, call.span.start);
        let Some(EvalValue::Obj(compiled)) = out.compiled.get(INLINE_NAMESPACE) else {
            return;
        };
        // The dev class name maps its key to itself, so this skips it.
        let hit = compiled.entries().find(|(key, value)| {
            *key != "$$css" && !matches!(value, EvalValue::Str(s) if s == key)
        });
        let Some((prop_key, EvalValue::Str(class_name))) = hit else {
            return;
        };
        let (prop_key, class_name) = (prop_key.to_string(), class_name.clone());
        let uid = self.generate_uid("temp", node);
        let anchor = self.state.program_statement_start(node);
        let text = self.splice_text(|| {
            format!(
                "const {uid} = {{{property}: _v => [{{{key}: _v != null ? {class} : _v, \"$$css\": true}}, {{{var}: _v != null ? _v : undefined}}]}};\n",
                key = js_string_literal(&prop_key),
                class = js_string_literal(&class_name),
                var = js_string_literal(&var_name),
            )
        });
        let hoist_idx = self.push_edit(Edit::insert(anchor, text));
        let callee_span = call.callee.span();
        let text = self.splice_text(|| format!("{uid}.{property}"));
        self.push_edit(Edit::replace(callee_span, text));
        self.atom_dynamic_callees.insert(callee_span);
        if self.build_plan {
            self.plan.inserts.push(InsertOp {
                anchor,
                seq: hoist_idx,
                span: call.span,
                stmt: SynthStmt::ConstDecl {
                    name: uid.clone(),
                    value: HoistValue::AtomDynamic {
                        property: property.to_string(),
                        prop_key,
                        class_name,
                        var_name,
                    },
                },
            });
            self.plan.replace_exprs.push((
                callee_span,
                SynthExpr::AtomCallee {
                    hoisted: uid,
                    property: property.to_string(),
                },
            ));
        }
    }

    // parity: scope.generateUid 7.29.8 — `_base`, `_base2`…`_base9`, `_base0`,
    // `_base1`, `_base10`, … skipping the babel collision surface.
    fn generate_uid(&mut self, base: &str, scope_node: oxc_syntax::node::NodeId) -> String {
        let mut i = 0usize;
        loop {
            let candidate = match i {
                0 => format!("_{base}"),
                1..=8 => format!("_{base}{}", i + 1),
                9 | 10 => format!("_{base}{}", i - 9),
                _ => format!("_{base}{}", i - 1),
            };
            if !self.generated_uids.contains(&candidate)
                && !self.state.uid_name_taken(&candidate)
                && !self.state.any_binding_at(scope_node, &candidate)
            {
                self.generated_uids.insert(candidate.clone());
                return candidate;
            }
            i += 1;
        }
    }

    // parity: index.js getStylexRuntimeBinding — per-site scope check against
    // the program-level import, then reuse of generated imports per site.
    fn stylex_local_for_site(
        &mut self,
        site: oxc_syntax::node::NodeId,
        site_offset: u32,
    ) -> String {
        let order = self.state.imports.stylex_namespace_order.clone();
        for name in &order {
            if self.state.resolves_to_root_binding(site, name) {
                return name.clone();
            }
        }
        // A generated import resolves at the site unless a source binding of
        // the same name shadows it (its own binding is registerDeclaration'd).
        let generated = self.sx_locals.clone();
        for name in &generated {
            if !self.state.any_binding_at(site, name) {
                return name.clone();
            }
        }
        let existing_source = self
            .import_decls
            .iter()
            .map(|(source, _)| source.clone())
            .find(|source| self.state.options.is_import_source(source));
        // The synthesized statement is always `import * as <local> from
        // "<from>"`, which an aliased source would not recognize on a re-run.
        let source = existing_source.clone().unwrap_or_else(|| {
            self.state
                .options
                .import_source_at(2)
                .or_else(|| self.state.options.import_source_at(0))
                .expect("the two built-in import sources are always present")
                .to_string()
        });
        let name = if existing_source.is_none()
            && self.sx_locals.is_empty()
            && !self.state.any_binding_at(site, "stylex")
        {
            "stylex".to_string()
        } else {
            self.generate_uid("stylex", site)
        };
        self.push_prologue_group(
            site_offset,
            vec![PrologueStmt::NamespaceImport {
                local: name.clone(),
                source,
            }],
        );
        self.sx_locals.push(name.clone());
        name
    }

    /// parity: ast-helpers.js addDefaultImport/addNamedImport — the import
    /// local is never called; an alias var beside it is.
    fn inject_callee(&mut self, site: oxc_syntax::node::NodeId, site_offset: u32) -> String {
        if let Some(alias) = &self.inject_alias {
            return alias.clone();
        }
        let injection = self
            .state
            .options
            .runtime_injection
            .clone()
            .expect("checked by the caller");
        let local = self.generate_uid("inject", site);
        let alias = match &injection.as_name {
            Some(as_name) => self.generate_uid(as_name, site),
            None => self.generate_uid("inject", site),
        };
        self.push_prologue_group(
            site_offset,
            vec![
                PrologueStmt::InjectImport {
                    local: local.clone(),
                    source: injection.from,
                    named: injection.as_name,
                },
                PrologueStmt::InjectAlias {
                    name: alias.clone(),
                    local,
                },
            ],
        );
        self.inject_alias = Some(alias.clone());
        alias
    }

    fn push_prologue_group(&mut self, site_offset: u32, group: Vec<PrologueStmt>) {
        self.prologue_groups.push((site_offset, group));
        let offset = self.import_insert_offset;
        let block = self.splice_text(|| {
            let block: String = self
                .prologue_statements()
                .iter()
                .map(prologue_text)
                .collect();
            // Empty program body: land on a fresh line after the directive prologue.
            if offset > 0 && !self.source[..offset as usize].ends_with('\n') {
                format!("\n{block}")
            } else {
                block
            }
        });
        let idx = self.prologue_edit.expect("slot 0 is reserved at init");
        self.edits[idx] = Some(Edit::insert(offset, block));
    }

    /// Replays babel's program.unshiftContainer per generating site in
    /// traversal (= source) order, then its `_blockHoist` body sort.
    fn prologue_statements(&self) -> Vec<PrologueStmt> {
        let mut groups: Vec<&(u32, Vec<PrologueStmt>)> = self.prologue_groups.iter().collect();
        groups.sort_by_key(|(offset, _)| *offset);
        let mut out: Vec<PrologueStmt> = Vec::new();
        for (_, group) in groups {
            for (i, stmt) in group.iter().enumerate() {
                out.insert(i, stmt.clone());
            }
        }
        out.sort_by_key(|stmt| std::cmp::Reverse(block_hoist(stmt)));
        out
    }

    // ---- pass B2: props()/attrs() and the sx spread ------------------------

    fn props_eval(&mut self, expr: &'a Expression<'a>) -> Result<EvalOutcome, StylexError> {
        let _t = crate::timings::start(crate::timings::Stage::Eval);
        let registry = self.props_registry.get_or_insert_with(|| {
            FunctionRegistry::for_props(&self.state.imports, &self.state.options.env)
        });
        let mut evaluator =
            Evaluator::with_registry_ref(&mut *self.state, self.fs, &RealCallables, registry, true);
        evaluator.eval(expr)
    }

    // parity: stylex-props.js parseNullableStyle.
    fn parse_nullable(&mut self, expr: &'a Expression<'a>) -> Result<NullableStyle, StylexError> {
        let expr = unwrap_parens(expr);
        match expr {
            Expression::NullLiteral(_) => return Ok(NullableStyle::Null),
            Expression::Identifier(id) if id.name == "undefined" => {
                return Ok(NullableStyle::Null);
            }
            Expression::CallExpression(call) => {
                // A marker call replaced in pass A is an object literal by now.
                if let Some(value) = self.replaced_values.get(&call.span.start) {
                    if matches!(value, EvalValue::Obj(_) | EvalValue::Arr(_)) {
                        return Ok(NullableStyle::Style(value.clone()));
                    }
                    return Ok(NullableStyle::Other);
                }
                return Ok(NullableStyle::Other);
            }
            Expression::StaticMemberExpression(_) | Expression::ComputedMemberExpression(_) => {
                // An atom the previous pass inlined is an object literal here.
                if let Some(site) = self.atom_site(expr.span()) {
                    return Ok(NullableStyle::Style(EvalValue::Obj(
                        self.atom_static_sites[site].compiled.clone(),
                    )));
                }
                if let Some((object_name, Some(prop))) = style_map_member(expr)
                    && let Some(namespaces) = self.style_map.get(&object_name)
                    && let Some(value) = namespaces.get(&prop)
                    && !matches!(value, EvalValue::Null | EvalValue::Undefined)
                {
                    return Ok(NullableStyle::Style(value.clone()));
                }
            }
            _ => {}
        }
        match self.props_eval(expr)? {
            EvalOutcome::NonStatic(_) => Ok(NullableStyle::Other),
            EvalOutcome::Value(JsValue::Proxy(_)) => Ok(NullableStyle::Other),
            EvalOutcome::Value(v) => {
                let value = into_eval_value(v);
                match &value {
                    EvalValue::Obj(map)
                        if map.get("__IS_PROXY") == Some(&EvalValue::Bool(true)) =>
                    {
                        Ok(NullableStyle::Other)
                    }
                    EvalValue::Obj(_) | EvalValue::Arr(_) => Ok(NullableStyle::Style(value)),
                    _ => Ok(NullableStyle::Other),
                }
            }
        }
    }

    /// parity: stylex-merge.js parseNullableStyle — node-based only, with no
    /// `evaluate()` fallback, so identifiers and object literals are 'other'.
    fn parse_nullable_legacy(&self, expr: &'a Expression<'a>) -> NullableStyle {
        let expr = unwrap_parens(expr);
        match expr {
            Expression::NullLiteral(_) => return NullableStyle::Null,
            Expression::Identifier(id) if id.name == "undefined" => return NullableStyle::Null,
            Expression::StaticMemberExpression(_) | Expression::ComputedMemberExpression(_) => {
                if let Some((object_name, Some(prop))) = style_map_member(expr)
                    && let Some(namespaces) = self.style_map.get(&object_name)
                    && let Some(value) = namespaces.get(&prop)
                    && !matches!(value, EvalValue::Null | EvalValue::Undefined)
                {
                    return NullableStyle::Style(value.clone());
                }
            }
            _ => {}
        }
        NullableStyle::Other
    }

    /// parity: stylex-merge.js `switch (arg.type)` over the *unflattened*
    /// arguments — no arrays, no identifiers, no object literals, no calls.
    fn classify_legacy(&mut self, arguments: &'a [Argument<'a>]) -> Classified {
        let mut args = Vec::with_capacity(arguments.len());
        let mut tests = Vec::new();
        for argument in arguments {
            let Some(expr) = argument.as_expression() else {
                args.push(MergeArg::Unsupported);
                continue;
            };
            match unwrap_parens(expr) {
                Expression::ConditionalExpression(c) => {
                    let primary = self.parse_nullable_legacy(&c.consequent);
                    let fallback = self.parse_nullable_legacy(&c.alternate);
                    if primary != NullableStyle::Other && fallback != NullableStyle::Other {
                        tests.push(c.test.span());
                    }
                    args.push(MergeArg::Conditional { primary, fallback });
                }
                Expression::LogicalExpression(l)
                    if l.operator == oxc_syntax::operator::LogicalOperator::And =>
                {
                    let left = self.parse_nullable_legacy(&l.left);
                    let right = self.parse_nullable_legacy(&l.right);
                    if left == NullableStyle::Other && right != NullableStyle::Other {
                        tests.push(l.left.span());
                    }
                    args.push(MergeArg::LogicalAnd { left, right });
                }
                Expression::LogicalExpression(_) => args.push(MergeArg::NonAndLogical),
                member @ (Expression::StaticMemberExpression(_)
                | Expression::ComputedMemberExpression(_)) => {
                    args.push(MergeArg::Resolved(self.parse_nullable_legacy(member)));
                }
                _ => args.push(MergeArg::Unsupported),
            }
        }
        Classified { args, tests }
    }

    fn classify(&mut self, flat: &[FlatArg<'a>]) -> Result<Classified, StylexError> {
        let mut args = Vec::with_capacity(flat.len());
        let mut tests = Vec::new();
        for entry in flat {
            let FlatArg::Expr(expr) = entry else {
                args.push(MergeArg::Unsupported);
                continue;
            };
            match expr {
                Expression::ConditionalExpression(c) => {
                    let primary = self.parse_nullable(&c.consequent)?;
                    let fallback = self.parse_nullable(&c.alternate)?;
                    if primary != NullableStyle::Other && fallback != NullableStyle::Other {
                        tests.push(c.test.span());
                    }
                    args.push(MergeArg::Conditional { primary, fallback });
                }
                Expression::LogicalExpression(l)
                    if l.operator == oxc_syntax::operator::LogicalOperator::And =>
                {
                    let left = self.parse_nullable(&l.left)?;
                    let right = self.parse_nullable(&l.right)?;
                    if left == NullableStyle::Other && right != NullableStyle::Other {
                        tests.push(l.left.span());
                    }
                    args.push(MergeArg::LogicalAnd { left, right });
                }
                Expression::LogicalExpression(_) => args.push(MergeArg::NonAndLogical),
                Expression::ObjectExpression(_)
                | Expression::Identifier(_)
                | Expression::StaticMemberExpression(_)
                | Expression::ComputedMemberExpression(_)
                | Expression::CallExpression(_) => {
                    args.push(MergeArg::Resolved(self.parse_nullable(expr)?));
                }
                _ => args.push(MergeArg::Unsupported),
            }
        }
        Ok(Classified { args, tests })
    }

    fn table_text(&self, entries: &[(u32, EvalValue)], tests: &[Span], inner: &[Edit]) -> String {
        use std::fmt::Write;
        let n = tests.len();
        let mut out = String::from("({");
        for (i, (key, leaf)) in entries.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "{key}: ");
            write_value(leaf, &mut out);
        }
        out.push_str("}[");
        for (i, span) in tests.iter().enumerate() {
            if i > 0 {
                out.push_str(" | ");
            }
            out.push_str("!!(");
            out.push_str(&render_span(self.source, *span, inner));
            let _ = write!(out, ") << {}", n - 1 - i);
        }
        out.push_str("])");
        out
    }

    fn process_props_call(
        &mut self,
        call: &'a CallExpression<'a>,
        parent: ExprParent<'_>,
        mode: MergeMode,
    ) -> Flow {
        let flat = flatten_call_args(&call.arguments);
        let classified = match self.classify(&flat) {
            Ok(c) => c,
            Err(error) => {
                self.fail(error);
                return Flow::Skip;
            }
        };
        let plan = match plan_merge(&classified.args, mode, self.state.options) {
            Ok(plan) => plan,
            Err(error) => {
                self.fail(error);
                return Flow::Skip;
            }
        };
        match plan {
            MergePlan::Bail { bail_out_index } => {
                if let Err(error) = self.collect_from_arguments(&call.arguments, bail_out_index) {
                    self.fail(error);
                    return Flow::Skip;
                }
                self.hoist_atoms_in_arguments(call);
                Flow::Walk
            }
            MergePlan::Inlined(map) => {
                if let ExprParent::JsxSpread(attr_span) = parent
                    && !map.is_empty()
                {
                    let _ = self.take_edits_within(attr_span);
                    let text = self.splice_text(|| jsx_attrs_text(&map));
                    self.push_edit(Edit::replace(attr_span, text));
                    if self.build_plan {
                        self.plan.jsx_ops.push((attr_span, JsxOp::Attrs(map)));
                    }
                } else {
                    let _ = self.take_edits_within(call.span);
                    let text = self.splice_text(|| print_parens(&map));
                    self.push_edit(Edit::replace(call.span, text));
                    if self.build_plan {
                        self.plan.replace_exprs.push((
                            call.span,
                            SynthExpr::ParenValue(EvalValue::Obj(Arc::new(map))),
                        ));
                    }
                }
                Flow::Skip
            }
            MergePlan::Table(entries) => {
                let entries = object_leaves(entries);
                let inner = self.take_edits_within(call.span);
                let text =
                    self.splice_text(|| self.table_text(&entries, &classified.tests, &inner));
                self.push_edit(Edit::replace(call.span, text));
                if self.build_plan {
                    self.plan.replace_exprs.push((
                        call.span,
                        SynthExpr::Table {
                            entries,
                            tests: classified.tests,
                        },
                    ));
                }
                Flow::Skip
            }
        }
    }

    // parity: stylex-merge.js — the merged value is `styleq(args)[0]`, emitted
    // as a bare string; there is no JSX-spread fast path and no bail hoisting.
    fn process_legacy_merge_call(&mut self, call: &'a CallExpression<'a>) -> Flow {
        let classified = self.classify_legacy(&call.arguments);
        let plan = match plan_legacy_merge(&classified.args, self.state.options) {
            Ok(plan) => plan,
            Err(error) => {
                self.fail(error);
                return Flow::Skip;
            }
        };
        match plan {
            MergePlan::Bail { bail_out_index } => {
                if let Err(error) = self.collect_from_arguments(&call.arguments, bail_out_index) {
                    self.fail(error);
                    return Flow::Skip;
                }
                Flow::Walk
            }
            MergePlan::Inlined(class_name) => {
                let _ = self.take_edits_within(call.span);
                let text = self.splice_text(|| js_string_literal(&class_name));
                self.push_edit(Edit::replace(call.span, text));
                if self.build_plan {
                    self.plan
                        .replace_exprs
                        .push((call.span, SynthExpr::Str(class_name)));
                }
                Flow::Skip
            }
            MergePlan::Table(entries) => {
                let entries: Vec<(u32, EvalValue)> = entries
                    .into_iter()
                    .map(|(key, class_name)| (key, EvalValue::Str(class_name)))
                    .collect();
                let inner = self.take_edits_within(call.span);
                let text =
                    self.splice_text(|| self.table_text(&entries, &classified.tests, &inner));
                self.push_edit(Edit::replace(call.span, text));
                if self.build_plan {
                    self.plan.replace_exprs.push((
                        call.span,
                        SynthExpr::Table {
                            entries,
                            tests: classified.tests,
                        },
                    ));
                }
                Flow::Skip
            }
        }
    }

    fn process_sx(&mut self, index: usize) -> Flow {
        let (attr_span, expr) = {
            let site = &self.sx_sites[index];
            (site.attr_span, site.expr)
        };
        let flat = flatten_single(expr);
        let classified = match self.classify(&flat) {
            Ok(c) => c,
            Err(error) => {
                self.fail(error);
                return Flow::Skip;
            }
        };
        let plan = match plan_merge(&classified.args, MergeMode::Props, self.state.options) {
            Ok(plan) => plan,
            Err(error) => {
                self.fail(error);
                return Flow::Skip;
            }
        };
        match plan {
            MergePlan::Bail { bail_out_index } => {
                if let Err(error) = self.collect_single(expr, bail_out_index) {
                    self.fail(error);
                    return Flow::Skip;
                }
                self.sx_sites[index].bailed.set(true);
                Flow::Walk
            }
            MergePlan::Inlined(map) => {
                let _ = self.take_edits_within(attr_span);
                let text = self.splice_text(|| {
                    if map.is_empty() {
                        "{...({})}".to_string()
                    } else {
                        jsx_attrs_text(&map)
                    }
                });
                self.push_edit(Edit::replace(attr_span, text));
                if self.build_plan {
                    let op = if map.is_empty() {
                        JsxOp::SpreadEmptyObject
                    } else {
                        JsxOp::Attrs(map)
                    };
                    self.plan.jsx_ops.push((attr_span, op));
                }
                Flow::Skip
            }
            MergePlan::Table(entries) => {
                let entries = object_leaves(entries);
                let inner = self.take_edits_within(attr_span);
                let text = self.splice_text(|| {
                    let table = self.table_text(&entries, &classified.tests, &inner);
                    format!("{{...{table}}}")
                });
                self.push_edit(Edit::replace(attr_span, text));
                if self.build_plan {
                    self.plan.jsx_ops.push((
                        attr_span,
                        JsxOp::SpreadTable {
                            entries,
                            tests: classified.tests,
                        },
                    ));
                }
                Flow::Skip
            }
        }
    }

    /// parity: stylex-props.js:277-300 — an argument object carrying `$$css:
    /// true` hoists to module scope. Atom-scoped here; see docs/design-core.md.
    fn hoist_atoms_in_arguments(&mut self, call: &'a CallExpression<'a>) {
        let anchor = self.state.program_statement_start(call.node_id.get());
        for argument in &call.arguments {
            let arg_span = argument.span();
            let sites: Vec<usize> = (0..self.atom_static_sites.len())
                .filter(|i| {
                    let site = &self.atom_static_sites[*i];
                    !site.hoisted
                        && site.span.start >= arg_span.start
                        && site.span.end <= arg_span.end
                })
                .collect();
            for index in sites {
                let site = &self.atom_static_sites[index];
                let (span, node_id, edit_idx, plan_idx) =
                    (site.span, site.node_id, site.edit_idx, site.plan_idx);
                let compiled = site.compiled.clone();
                let uid = self.generate_uid("temp", node_id);
                let text =
                    self.splice_text(|| format!("const {uid} = {};\n", print_parens(&compiled)));
                let hoist_idx = self.push_edit(Edit::insert(anchor, text));
                self.edits[edit_idx] = Some(Edit::replace(span, uid.clone()));
                if self.build_plan {
                    if let Some(plan_idx) = plan_idx {
                        self.plan.replace_exprs[plan_idx] = (span, SynthExpr::Ident(uid.clone()));
                    }
                    self.plan.inserts.push(InsertOp {
                        anchor,
                        seq: hoist_idx,
                        span,
                        stmt: SynthStmt::ConstDecl {
                            name: uid,
                            value: HoistValue::ParenValue(EvalValue::Obj(compiled)),
                        },
                    });
                }
                self.atom_static_sites[index].hoisted = true;
            }
        }
    }

    fn collect_from_arguments(
        &mut self,
        arguments: &'a [Argument<'a>],
        bail_out_index: Option<usize>,
    ) -> Result<(), StylexError> {
        let mut collector = StyleVarsCollector::new(bail_out_index);
        for (index, argument) in arguments.iter().enumerate() {
            let expr = match argument {
                Argument::SpreadElement(spread) => &spread.argument,
                other => match other.as_expression() {
                    Some(expr) => expr,
                    None => continue,
                },
            };
            self.collect_members(expr, index, &mut collector)?;
        }
        Ok(())
    }

    fn collect_single(
        &mut self,
        expr: &'a Expression<'a>,
        bail_out_index: Option<usize>,
    ) -> Result<(), StylexError> {
        let mut collector = StyleVarsCollector::new(bail_out_index);
        self.collect_members(expr, 0, &mut collector)
    }

    fn collect_members(
        &mut self,
        expr: &'a Expression<'a>,
        index: usize,
        collector: &mut StyleVarsCollector,
    ) -> Result<(), StylexError> {
        let mut hooks = CollectorHooks {
            s: self,
            collector,
            index,
            error: None,
        };
        walk_expression(&mut hooks, expr, ExprParent::Other);
        match hooks.error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn eval_member_for_collector(
        &mut self,
        expr: &'a Expression<'a>,
    ) -> Result<MemberEval, StylexError> {
        match self.props_eval(expr)? {
            EvalOutcome::NonStatic(_) => Ok(MemberEval::NonStatic),
            EvalOutcome::Value(JsValue::Proxy(_)) => Ok(MemberEval::NonStatic),
            EvalOutcome::Value(v) if is_nullish(&v) => Ok(MemberEval::NonStatic),
            EvalOutcome::Value(v) => {
                let value = into_eval_value(v);
                if let EvalValue::Obj(map) = &value
                    && map.get("__IS_PROXY") == Some(&EvalValue::Bool(true))
                {
                    return Ok(MemberEval::NonStatic);
                }
                Ok(MemberEval::Value(value))
            }
        }
    }

    // ---- finalization -------------------------------------------------------

    fn finalize_sx_bails(&mut self) {
        let sites = std::mem::take(&mut self.sx_sites);
        for site in sites.iter().filter(|s| s.bailed.get()) {
            let inner = self.take_edits_within(site.attr_span);
            let text = self.splice_text(|| {
                let rendered = render_span(self.source, site.expr.span(), &inner);
                format!("{{...{}.props({})}}", site.local, rendered)
            });
            self.push_edit(Edit::replace(site.attr_span, text));
            if self.build_plan {
                self.plan.jsx_ops.push((
                    site.attr_span,
                    JsxOp::SpreadProps {
                        local: site.local.clone(),
                        expr_span: site.expr.span(),
                    },
                ));
            }
        }
    }

    fn apply_dce(&mut self) {
        let vars = compute_vars_to_keep(&self.tuples);
        // Taken to borrow past the edit/plan mutations; restored for the
        // caller's create_objects extraction.
        let sites = std::mem::take(&mut self.create_sites);
        let mut hoisted: BTreeSet<usize> = sites
            .iter()
            .flat_map(|s| s.hoist_edit_idxs.iter().copied())
            .collect();
        // The prologue lands at the body start and inject calls sit at their
        // anchor's start, which a removed create's span would else swallow.
        hoisted.extend(self.prologue_edit);
        hoisted.extend(self.inject_edits.iter().copied());
        type StmtRemovals = (Span, Rc<[Span]>, BTreeSet<usize>);
        let mut removals: BTreeMap<u32, StmtRemovals> = BTreeMap::new();
        for site in &sites {
            if !site.style_var {
                continue;
            }
            let name = site.var_name.as_deref().expect("style vars have names");
            match dce_action(name, &site.compiled, site.exported, &vars, &self.tuples) {
                DceAction::KeepAll => {}
                DceAction::Prune(map) => {
                    let live = self.edits.get(site.edit_idx).is_some_and(Option::is_some);
                    if live {
                        let text = self.splice_text(|| print_create_parens(&map, &site.dynamic));
                        self.edits[site.edit_idx] = Some(Edit::replace(site.span, text));
                        if let Some(plan_idx) = site.plan_idx
                            && let Some((_, synth)) = self.plan.replace_exprs.get_mut(plan_idx)
                        {
                            *synth = SynthExpr::CreateObject {
                                map: Arc::new(map),
                                dynamic: site.dynamic_entries.clone(),
                            };
                        }
                    }
                }
                DceAction::Remove => {
                    let removal = site
                        .removal
                        .as_ref()
                        .expect("style vars are top-level declarators");
                    let entry = removals.entry(removal.stmt_span.start).or_insert_with(|| {
                        (
                            removal.stmt_span,
                            Rc::clone(&removal.decl_spans),
                            BTreeSet::new(),
                        )
                    });
                    entry.2.insert(removal.decl_index);
                }
            }
        }
        for (stmt_span, decl_spans, removed) in removals.into_values() {
            if self.build_plan {
                self.plan.removes.push(RemoveOp {
                    stmt_span,
                    decl_count: decl_spans.len(),
                    indices: removed.iter().copied().collect(),
                });
            }
            for span in plan_removal_spans(stmt_span, &decl_spans, &removed) {
                for (idx, slot) in self.edits.iter_mut().enumerate() {
                    if hoisted.contains(&idx) {
                        continue;
                    }
                    if let Some(edit) = slot
                        && edit.start >= span.start
                        && edit.end <= span.end
                    {
                        *slot = None;
                    }
                }
                self.edits.push(Some(Edit::replace(span, String::new())));
            }
        }
        self.create_sites = sites;
    }

    /// `rewriteAliases`: every import source that resolves through the option
    /// becomes a `./`-prefixed relative path, extension stripped.
    fn rewrite_import_sources(&mut self, program: &Program<'a>) -> BTreeMap<String, String> {
        let mut rewrites: BTreeMap<String, String> = BTreeMap::new();
        if !self.state.options.rewrite_aliases {
            return rewrites;
        }
        let Some(filename) = self.state.filename.clone() else {
            return rewrites;
        };
        for statement in &program.body {
            let Statement::ImportDeclaration(decl) = statement else {
                continue;
            };
            let source = decl.source.value.as_str();
            let rewritten = match rewrites.get(source) {
                Some(hit) => hit.clone(),
                None => {
                    let Some(rewritten) = rewritten_import_source(
                        self.fs,
                        source,
                        Path::new(&filename),
                        self.state.options,
                    ) else {
                        continue;
                    };
                    rewrites.insert(source.to_string(), rewritten.clone());
                    rewritten
                }
            };
            let text = self.splice_text(|| js_string_literal(&rewritten));
            self.push_edit(Edit::replace(decl.source.span, text));
            if self.build_plan {
                self.plan.import_sources.push((decl.source.span, rewritten));
            }
        }
        rewrites
    }

    // parity: evaluate-path.js importPath.insertBefore — the exact declaring
    // import; an unrewritten source node reprints with its raw quotes.
    fn insert_treeshake_imports(&mut self, rewrites: &BTreeMap<String, String>) {
        let imports = self.state.treeshake_imports.clone();
        for import in imports {
            let text = self.splice_text(|| {
                let raw = match rewrites.get(&import.specifier) {
                    Some(rewritten) => js_string_literal(rewritten),
                    None => render_span(self.source, import.source_span, &[]),
                };
                format!("import {raw};\n")
            });
            let idx = self.push_edit(Edit::insert(import.decl_start, text));
            if self.build_plan {
                self.plan.inserts.push(InsertOp {
                    anchor: import.decl_start,
                    seq: idx,
                    span: Span::new(import.decl_start, import.source_span.end),
                    stmt: SynthStmt::SideEffectImport {
                        specifier: rewrites
                            .get(&import.specifier)
                            .unwrap_or(&import.specifier)
                            .clone(),
                        source_span: import.source_span,
                    },
                });
            }
        }
    }
}

// parity: babel path.remove() per dead declarator — kept declarators survive
// with separators intact; an all-dead statement is removed whole.
fn plan_removal_spans(
    stmt_span: Span,
    decl_spans: &[Span],
    removed: &BTreeSet<usize>,
) -> Vec<Span> {
    if removed.len() == decl_spans.len() {
        return vec![stmt_span];
    }
    let indices: Vec<usize> = removed.iter().copied().collect();
    let mut out = Vec::new();
    let mut k = 0;
    while k < indices.len() {
        let start_idx = indices[k];
        let mut end_idx = start_idx;
        while k + 1 < indices.len() && indices[k + 1] == end_idx + 1 {
            k += 1;
            end_idx = indices[k];
        }
        out.push(if end_idx + 1 < decl_spans.len() {
            Span::new(decl_spans[start_idx].start, decl_spans[end_idx + 1].start)
        } else {
            Span::new(decl_spans[start_idx - 1].end, decl_spans[end_idx].end)
        });
        k += 1;
    }
    out
}

fn object_leaves(entries: Vec<(u32, JsObjectMap)>) -> Vec<(u32, EvalValue)> {
    entries
        .into_iter()
        .map(|(key, map)| (key, EvalValue::Obj(Arc::new(map))))
        .collect()
}

fn style_map_member(expr: &Expression<'_>) -> Option<(String, Option<String>)> {
    match expr {
        Expression::StaticMemberExpression(m) => match unwrap_parens(&m.object) {
            Expression::Identifier(obj) => {
                Some((obj.name.to_string(), Some(m.property.name.to_string())))
            }
            _ => None,
        },
        Expression::ComputedMemberExpression(m) => match unwrap_parens(&m.object) {
            Expression::Identifier(obj) => {
                let prop = match unwrap_parens(&m.expression) {
                    Expression::StringLiteral(s) => Some(s.value.to_string()),
                    Expression::NumericLiteral(n) => Some(js_number_to_string(n.value)),
                    _ => None,
                };
                Some((obj.name.to_string(), prop))
            }
            _ => None,
        },
        _ => None,
    }
}

fn flatten_call_args<'a>(arguments: &'a [Argument<'a>]) -> Vec<FlatArg<'a>> {
    let mut flat = Vec::new();
    for argument in arguments {
        match argument {
            Argument::SpreadElement(_) => flat.push(FlatArg::Bad),
            other => match other.as_expression() {
                Some(expr) => flatten_into(unwrap_parens(expr), &mut flat),
                None => flat.push(FlatArg::Bad),
            },
        }
    }
    flat
}

fn flatten_single<'a>(expr: &'a Expression<'a>) -> Vec<FlatArg<'a>> {
    let mut flat = Vec::new();
    flatten_into(unwrap_parens(expr), &mut flat);
    flat
}

// parity: stylex-props.js flatMap — exactly one level of array expansion.
fn flatten_into<'a>(expr: &'a Expression<'a>, flat: &mut Vec<FlatArg<'a>>) {
    if let Expression::ArrayExpression(arr) = expr {
        for element in &arr.elements {
            match element {
                ArrayExpressionElement::Elision(_) => flat.push(FlatArg::Bad),
                ArrayExpressionElement::SpreadElement(_) => flat.push(FlatArg::Bad),
                other => match other.as_expression() {
                    Some(e) => flat.push(FlatArg::Expr(unwrap_parens(e))),
                    None => flat.push(FlatArg::Bad),
                },
            }
        }
    } else {
        flat.push(FlatArg::Expr(expr));
    }
}

// ---------------------------------------------------------------------------
// Pass A: main-traversal transforms (imports were scanned at state build)

struct PassA<'w, 'a, 'env> {
    s: &'w mut Shared<'a, 'env>,
    index: SiteIndex,
}

impl<'a> Hooks<'a> for PassA<'_, 'a, '_> {
    fn done(&self) -> bool {
        self.s.error.is_some()
    }

    fn wants(&self, span: Span) -> bool {
        self.index.any_in(span)
    }

    fn call(&mut self, call: &'a CallExpression<'a>, parent: ExprParent<'_>) -> Flow {
        let Some(kind) = call_kind(call, self.s.state) else {
            return Flow::Walk;
        };
        match kind {
            CallKind::Keyframes => match parent {
                ExprParent::Declarator(ctx) if ctx.name.is_some() => {
                    let ctx = ctx.clone();
                    self.s.transform_keyframes(call, &ctx);
                    Flow::Skip
                }
                _ => Flow::Walk,
            },
            CallKind::PositionTry => match parent {
                ExprParent::Declarator(ctx) if ctx.name.is_some() => {
                    let ctx = ctx.clone();
                    self.s.transform_position_try(call, &ctx);
                    Flow::Skip
                }
                _ => Flow::Walk,
            },
            CallKind::ViewTransitionClass => match parent {
                ExprParent::Declarator(ctx) if ctx.name.is_some() => {
                    let ctx = ctx.clone();
                    self.s.transform_view_transition(call, &ctx);
                    Flow::Skip
                }
                _ => Flow::Walk,
            },
            CallKind::DefaultMarker => {
                self.s.transform_default_marker(call, parent);
                Flow::Skip
            }
            CallKind::DefineMarker => {
                self.s.transform_define_marker(call, parent);
                Flow::Skip
            }
            CallKind::DefineVars => {
                self.s.transform_define_vars(call, parent);
                Flow::Skip
            }
            CallKind::CreateTheme => {
                self.s.transform_create_theme(call, parent);
                Flow::Skip
            }
            CallKind::DefineVarsNested => {
                self.s.transform_define_vars_nested(call, parent);
                Flow::Skip
            }
            CallKind::DefineConstsNested => {
                self.s.transform_define_consts_nested(call, parent);
                Flow::Skip
            }
            CallKind::CreateThemeNested => {
                self.s.transform_create_theme_nested(call, parent);
                Flow::Skip
            }
            CallKind::DefineConsts => {
                self.s.transform_define_consts(call, parent);
                Flow::Skip
            }
            CallKind::Create => {
                self.s.transform_create(call, parent);
                Flow::Skip
            }
            CallKind::Props | CallKind::Attrs | CallKind::LegacyMerge => Flow::Walk,
        }
    }

    fn jsx_opening(&mut self, el: &'a JSXOpeningElement<'a>) {
        let Some(sx_name) = self.s.state.options.sx_prop_name.as_deref() else {
            return;
        };
        let JSXElementName::Identifier(name) = &el.name else {
            return;
        };
        let Some(first) = name.name.chars().next() else {
            return;
        };
        // babel's lowercase-element test, allocation-free: the char is its own
        // lowercase mapping.
        if !first.to_lowercase().eq(std::iter::once(first)) {
            return;
        }
        for item in &el.attributes {
            let JSXAttributeItem::Attribute(attr) = item else {
                continue;
            };
            let JSXAttributeName::Identifier(attr_name) = &attr.name else {
                continue;
            };
            if attr_name.name != sx_name {
                continue;
            }
            let Some(JSXAttributeValue::ExpressionContainer(container)) = &attr.value else {
                continue;
            };
            let Some(expr) = container.expression.as_expression() else {
                continue;
            };
            let local = self
                .s
                .stylex_local_for_site(el.node_id.get(), el.span.start);
            self.s.sx_sites.push(SxSite {
                attr_span: attr.span,
                expr,
                local,
                bailed: Cell::new(false),
            });
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Pass B1: Program.exit identifier walk (composed-namespace marking)

struct PassB1<'w, 'a, 'env> {
    s: &'w mut Shared<'a, 'env>,
    index: SiteIndex,
}

impl<'a> Shared<'a, '_> {
    fn b1_call(&self, call: &'a CallExpression<'a>) -> Flow {
        // Dead spans still walk when they carry live (spliced) sub-spans; the
        // identifier/member hooks re-check per node.
        if self.in_dead(call.span) && !self.contains_live(call.span) {
            return Flow::Skip;
        }
        match call_kind(call, self.state) {
            Some(CallKind::Props | CallKind::Attrs | CallKind::LegacyMerge) => Flow::Skip,
            _ => Flow::Walk,
        }
    }

    fn b1_jsx_attribute(&self, attr: &'a JSXAttribute<'a>) -> Flow {
        if self.sx_sites.iter().any(|s| s.attr_span == attr.span) {
            Flow::Skip
        } else {
            Flow::Walk
        }
    }

    fn b1_identifier(&mut self, name: &str, span: Span) {
        if self.in_dead(span) || !self.style_map.contains_key(name) {
            return;
        }
        self.tuples.push(StyleVarToKeep {
            var_name: name.to_string(),
            namespace: None,
            non_null_props: crate::transform::merge::NonNullProps::True,
        });
    }

    fn b1_member(&mut self, info: &MemberInfo<'a>) -> MemberFlow {
        if info.optional || self.in_dead(info.span) {
            return MemberFlow::Walk;
        }
        let Expression::Identifier(object) = unwrap_parens(info.object) else {
            return MemberFlow::Walk;
        };
        if !self.style_map.contains_key(object.name.as_str()) {
            return MemberFlow::Walk;
        }
        let namespace = match &info.prop {
            MemberProp::Static(name) => Some((*name).to_string()),
            MemberProp::Computed(expr) => match unwrap_parens(expr) {
                Expression::StringLiteral(s) => Some(s.value.to_string()),
                Expression::NumericLiteral(n) => Some(js_number_to_string(n.value)),
                _ => None,
            },
        };
        self.tuples.push(StyleVarToKeep {
            var_name: object.name.to_string(),
            namespace,
            non_null_props: crate::transform::merge::NonNullProps::True,
        });
        MemberFlow::SkipObject
    }

    fn b2_call(&mut self, call: &'a CallExpression<'a>, parent: ExprParent<'_>) -> Flow {
        if self.in_dead(call.span) {
            return Flow::Skip;
        }
        match call_kind(call, self.state) {
            Some(CallKind::LegacyMerge) => self.process_legacy_merge_call(call),
            Some(CallKind::Props) => self.process_props_call(call, parent, MergeMode::Props),
            Some(CallKind::Attrs) => self.process_props_call(call, parent, MergeMode::Attrs),
            _ => Flow::Walk,
        }
    }

    fn b2_jsx_attribute(&mut self, attr: &'a JSXAttribute<'a>) -> Flow {
        let index = self.sx_sites.iter().position(|s| s.attr_span == attr.span);
        match index {
            Some(index) => self.process_sx(index),
            None => Flow::Walk,
        }
    }
}

impl<'a> Hooks<'a> for PassB1<'_, 'a, '_> {
    fn done(&self) -> bool {
        false
    }

    fn wants(&self, span: Span) -> bool {
        self.index.any_in(span)
    }

    fn call(&mut self, call: &'a CallExpression<'a>, _parent: ExprParent<'_>) -> Flow {
        self.s.b1_call(call)
    }

    fn jsx_attribute(&mut self, attr: &'a JSXAttribute<'a>) -> Flow {
        self.s.b1_jsx_attribute(attr)
    }

    fn identifier(&mut self, name: &str, span: Span) {
        self.s.b1_identifier(name, span);
    }

    fn member(&mut self, info: &MemberInfo<'a>) -> MemberFlow {
        self.s.b1_member(info)
    }

    fn export_local(&mut self, name: &str, _span: Span) {
        self.s.b1_identifier(name, Span::default());
    }
}

// ---------------------------------------------------------------------------
// Pass Atoms: Program.exit pass 2, between the identifier walk and props/attrs

struct PassAtoms<'w, 'a, 'env> {
    s: &'w mut Shared<'a, 'env>,
    /// Member spans sitting in callee position; the static form skips those.
    callee_spans: BTreeSet<Span>,
}

impl<'a> Hooks<'a> for PassAtoms<'_, 'a, '_> {
    fn done(&self) -> bool {
        self.s.error.is_some()
    }

    fn call(&mut self, call: &'a CallExpression<'a>, _parent: ExprParent<'_>) -> Flow {
        if self.s.within_dead_span(call.span) {
            return Flow::Skip;
        }
        self.callee_spans.insert(call.callee.span());
        match dynamic_style(call, call.node_id.get(), self.s.state) {
            Some((AtomStyle::Dynamic { property }, _arg)) => {
                self.s.transform_atom_dynamic(call, &property);
                Flow::ArgsOnly
            }
            _ => Flow::Walk,
        }
    }

    fn member(&mut self, info: &MemberInfo<'a>) -> MemberFlow {
        if info.optional
            || self.callee_spans.contains(&info.span)
            || self.s.within_dead_span(info.span)
        {
            return MemberFlow::Walk;
        }
        let key = match &info.prop {
            MemberProp::Static(name) => Some((*name).to_string()),
            MemberProp::Computed(expr) => match expr {
                Expression::StringLiteral(s) => Some(s.value.to_string()),
                Expression::NumericLiteral(n) => Some(js_number_to_string(n.value)),
                _ => None,
            },
        };
        match static_style(info.object, key.as_deref(), info.node_id, self.s.state) {
            Some(AtomStyle::Static { property, value }) => {
                self.s.transform_atom_static(info, &property, &value);
                MemberFlow::SkipObject
            }
            _ => MemberFlow::Walk,
        }
    }
}

// ---------------------------------------------------------------------------
// Pass B2: Program.exit call transforms (legacy merge ban, props/attrs, sx)

struct PassB2<'w, 'a, 'env> {
    s: &'w mut Shared<'a, 'env>,
    index: SiteIndex,
}

impl<'a> Hooks<'a> for PassB2<'_, 'a, '_> {
    fn done(&self) -> bool {
        self.s.error.is_some()
    }

    fn wants(&self, span: Span) -> bool {
        self.index.any_in(span)
    }

    fn call(&mut self, call: &'a CallExpression<'a>, parent: ExprParent<'_>) -> Flow {
        self.s.b2_call(call, parent)
    }

    fn jsx_attribute(&mut self, attr: &'a JSXAttribute<'a>) -> Flow {
        self.s.b2_jsx_attribute(attr)
    }
}

// ---------------------------------------------------------------------------
// Pass B1 + B2 in one walk (no atoms pass between them)

/// Each pass keeps its own skipped subtree: the walk descends when either
/// pass wants to, and a pass's hooks stay silent inside the span it skipped.
struct PassB12<'w, 'a, 'env> {
    s: &'w mut Shared<'a, 'env>,
    index: SiteIndex,
    b1_skip: Option<Span>,
    b2_skip: Option<Span>,
}

fn within(skip: Option<Span>, span: Span) -> bool {
    skip.is_some_and(|s| s.start <= span.start && span.end <= s.end)
}

fn skip_or_walk(flow: Flow, skip: &mut Option<Span>, span: Span, walk: &mut bool) {
    match flow {
        Flow::Skip => *skip = Some(span),
        Flow::Walk | Flow::ArgsOnly => *walk = true,
    }
}

impl<'a> Hooks<'a> for PassB12<'_, 'a, '_> {
    fn done(&self) -> bool {
        self.s.error.is_some()
    }

    fn wants(&self, span: Span) -> bool {
        self.index.any_in(span)
    }

    fn call(&mut self, call: &'a CallExpression<'a>, parent: ExprParent<'_>) -> Flow {
        let mut walk = false;
        if !within(self.b1_skip, call.span) {
            let flow = self.s.b1_call(call);
            skip_or_walk(flow, &mut self.b1_skip, call.span, &mut walk);
        }
        if !within(self.b2_skip, call.span) {
            let flow = self.s.b2_call(call, parent);
            skip_or_walk(flow, &mut self.b2_skip, call.span, &mut walk);
        }
        if walk { Flow::Walk } else { Flow::Skip }
    }

    fn jsx_attribute(&mut self, attr: &'a JSXAttribute<'a>) -> Flow {
        let mut walk = false;
        if !within(self.b1_skip, attr.span) {
            let flow = self.s.b1_jsx_attribute(attr);
            skip_or_walk(flow, &mut self.b1_skip, attr.span, &mut walk);
        }
        if !within(self.b2_skip, attr.span) {
            let flow = self.s.b2_jsx_attribute(attr);
            skip_or_walk(flow, &mut self.b2_skip, attr.span, &mut walk);
        }
        if walk { Flow::Walk } else { Flow::Skip }
    }

    fn identifier(&mut self, name: &str, span: Span) {
        if !within(self.b1_skip, span) {
            self.s.b1_identifier(name, span);
        }
    }

    fn member(&mut self, info: &MemberInfo<'a>) -> MemberFlow {
        if within(self.b1_skip, info.span) {
            MemberFlow::Walk
        } else {
            self.s.b1_member(info)
        }
    }

    fn export_local(&mut self, name: &str, _span: Span) {
        self.s.b1_identifier(name, Span::default());
    }
}

// ---------------------------------------------------------------------------
// Bail-branch member collector (runs inside pass B2)

struct CollectorHooks<'w, 'x, 'a, 'env> {
    s: &'w mut Shared<'a, 'env>,
    collector: &'x mut StyleVarsCollector,
    index: usize,
    error: Option<StylexError>,
}

impl<'a> Hooks<'a> for CollectorHooks<'_, '_, 'a, '_> {
    fn done(&self) -> bool {
        self.error.is_some()
    }

    fn member(&mut self, info: &MemberInfo<'a>) -> MemberFlow {
        if info.optional {
            return MemberFlow::Walk;
        }
        // Post-atoms the node is an object literal, so no member is visited.
        if self.s.atom_site(info.span).is_some() {
            return MemberFlow::SkipObject;
        }
        // A dynamic atom's callee reads as `_temp.<prop>`: never a style map
        // member, and never statically evaluable.
        if self.s.atom_dynamic_callees.contains(&info.span) {
            self.collector
                .record(self.index, None, || MemberEval::NonStatic);
            return MemberFlow::SkipObject;
        }
        let member = style_map_member_info(info);
        let style_member = member
            .as_ref()
            .filter(|(object, _)| self.s.style_map.contains_key(object));
        let member_expr = info.expr;
        let mut eval_error: Option<StylexError> = None;
        let s = &mut *self.s;
        let kept = self.collector.record(
            self.index,
            style_member.map(|(object, prop)| (object.as_str(), prop.as_deref())),
            || match member_expr {
                None => MemberEval::NonStatic,
                Some(expr) => match s.eval_member_for_collector(expr) {
                    Ok(result) => result,
                    Err(error) => {
                        eval_error = Some(error);
                        MemberEval::NonStatic
                    }
                },
            },
        );
        if let Some(error) = eval_error {
            self.error = Some(error);
            return MemberFlow::Walk;
        }
        if let Some(tuple) = kept {
            self.s.tuples.push(tuple);
        }
        MemberFlow::Walk
    }
}

fn style_map_member_info(info: &MemberInfo<'_>) -> Option<(String, Option<String>)> {
    let Expression::Identifier(object) = unwrap_parens(info.object) else {
        return None;
    };
    let prop = match &info.prop {
        MemberProp::Static(name) => Some((*name).to_string()),
        MemberProp::Computed(expr) => match unwrap_parens(expr) {
            Expression::StringLiteral(s) => Some(s.value.to_string()),
            Expression::NumericLiteral(n) => Some(js_number_to_string(n.value)),
            _ => None,
        },
    };
    Some((object.name.to_string(), prop))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn removal_spans_are_disjoint_per_statement() {
        // const a = 1, b = 2, c = 3;  (statement 0..26, declarators below)
        let stmt = Span::new(0, 26);
        let decls = [Span::new(6, 11), Span::new(13, 18), Span::new(20, 25)];
        let plan = |idxs: &[usize]| {
            plan_removal_spans(stmt, &decls, &idxs.iter().copied().collect::<BTreeSet<_>>())
        };
        assert_eq!(plan(&[0, 1, 2]), vec![stmt]);
        assert_eq!(plan(&[0]), vec![Span::new(6, 13)]);
        assert_eq!(plan(&[1]), vec![Span::new(13, 20)]);
        assert_eq!(plan(&[2]), vec![Span::new(18, 25)]);
        assert_eq!(plan(&[0, 2]), vec![Span::new(6, 13), Span::new(18, 25)]);
        assert_eq!(plan(&[0, 1]), vec![Span::new(6, 20)]);
        assert_eq!(plan(&[1, 2]), vec![Span::new(11, 25)]);
        for idxs in [&[0usize, 2][..], &[0, 1], &[1, 2]] {
            let spans = plan(idxs);
            for pair in spans.windows(2) {
                assert!(pair[0].end <= pair[1].start, "overlap: {spans:?}");
            }
        }
    }

    struct IdentifierRecorder {
        index: Option<SiteIndex>,
        seen: Vec<(String, u32)>,
    }

    impl Hooks<'_> for IdentifierRecorder {
        fn done(&self) -> bool {
            false
        }

        fn wants(&self, span: Span) -> bool {
            self.index.as_ref().is_none_or(|index| index.any_in(span))
        }

        fn identifier(&mut self, name: &str, span: Span) {
            self.seen.push((name.to_string(), span.start));
        }
    }

    pub(crate) fn corpus_sources() -> Vec<(String, String)> {
        let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../conformance"));
        let mut out = Vec::new();
        // Vendored copies of this crate ship without the conformance corpus.
        let Ok(dirs) = std::fs::read_dir(root.join("corpus")) else {
            return out;
        };
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir.unwrap().path()) else {
                continue;
            };
            for entry in entries {
                let path = entry.unwrap().path();
                if !path.to_string_lossy().ends_with(".jobs.json") {
                    continue;
                }
                let parsed: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
                for job in parsed["jobs"].as_array().expect("jobs array") {
                    // `sourceFile` corpora are fetched on demand; skip what is absent.
                    let source = match job.get("source").and_then(|s| s.as_str()) {
                        Some(source) => source.to_string(),
                        None => match std::fs::read_to_string(
                            root.join(job["sourceFile"].as_str().expect("source or sourceFile")),
                        ) {
                            Ok(source) => source,
                            Err(_) => continue,
                        },
                    };
                    let filename = job["filename"].as_str().unwrap_or("input.tsx");
                    out.push((filename.to_string(), source));
                }
            }
        }
        out
    }

    struct PassACallRecorder<'s, 'a> {
        state: &'s CompileState<'a>,
        spans: Vec<Span>,
    }

    impl<'a> Hooks<'a> for PassACallRecorder<'_, 'a> {
        fn done(&self) -> bool {
            false
        }

        fn call(&mut self, call: &'a CallExpression<'a>, _parent: ExprParent<'_>) -> Flow {
            if !matches!(
                call_kind(call, self.state),
                None | Some(CallKind::Props | CallKind::Attrs | CallKind::LegacyMerge)
            ) {
                self.spans.push(call.span);
            }
            Flow::Walk
        }
    }

    /// Pass A's index, filtered to API-member callees, still opens every call
    /// its hook acts on.
    #[test]
    fn pass_a_index_opens_every_pass_a_call() {
        let options = crate::options::CompilerOptions::from_json(&serde_json::json!({}))
            .unwrap()
            .resolve()
            .unwrap();
        let mut checked = 0usize;
        let sources = corpus_sources();
        if sources.is_empty() {
            eprintln!(
                "skipping pass_a_index_opens_every_pass_a_call: conformance corpus not vendored"
            );
            return;
        }
        for (filename, source) in sources {
            let allocator = oxc_allocator::Allocator::default();
            let Ok(program) = crate::api::parse_program(&allocator, &source, &filename) else {
                continue;
            };
            let Ok(state) = CompileState::build(&program, &options, None, String::new()) else {
                continue;
            };
            let index = pass_a_index(&state, &source);
            let mut recorder = PassACallRecorder {
                state: &state,
                spans: Vec::new(),
            };
            walk_program(&mut recorder, &program);
            for span in recorder.spans {
                assert!(
                    index.any_in(span),
                    "{filename}: pass-A call at {} is not indexed",
                    span.start
                );
                checked += 1;
            }
        }
        assert!(checked > 500, "only {checked} calls checked");
    }

    /// A walk pruned by one name's reference index fires the identifier hook
    /// for that name exactly where the unpruned walk does.
    #[test]
    fn pruned_walk_reaches_every_identifier_of_the_indexed_name() {
        let options = crate::options::CompilerOptions::from_json(&serde_json::json!({}))
            .unwrap()
            .resolve()
            .unwrap();
        let mut checked = 0usize;
        let sources = corpus_sources();
        if sources.is_empty() {
            eprintln!(
                "skipping pruned_walk_reaches_every_identifier_of_the_indexed_name: conformance corpus not vendored"
            );
            return;
        }
        for (filename, source) in sources {
            let allocator = oxc_allocator::Allocator::default();
            let Ok(program) = crate::api::parse_program(&allocator, &source, &filename) else {
                continue;
            };
            let state = CompileState::build_with_imports(
                &program,
                &options,
                None,
                String::new(),
                crate::imports::ImportTable::default(),
            );
            let mut full = IdentifierRecorder {
                index: None,
                seen: Vec::new(),
            };
            walk_program(&mut full, &program);
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            for (name, _) in &full.seen {
                *counts.entry(name.as_str()).or_default() += 1;
            }
            let mut names: Vec<(&str, usize)> = counts.into_iter().collect();
            names.sort_by_key(|&(name, count)| (count, name));
            for (name, _) in names.into_iter().take(30) {
                let mut pruned = IdentifierRecorder {
                    index: Some(SiteIndex {
                        starts: state.reference_starts(&[name]),
                    }),
                    seen: Vec::new(),
                };
                walk_program(&mut pruned, &program);
                let of_name = |seen: &[(String, u32)]| -> Vec<u32> {
                    seen.iter()
                        .filter(|(n, _)| n == name)
                        .map(|(_, start)| *start)
                        .collect()
                };
                assert_eq!(
                    of_name(&pruned.seen),
                    of_name(&full.seen),
                    "{filename}: pruned walk misses `{name}`"
                );
                checked += 1;
            }
        }
        assert!(checked > 1_000, "only {checked} names checked");
    }
}
