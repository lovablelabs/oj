//! Oracle-pinned dynamic-namespace tests: rules/@property replay exactly; fn
//! semantics proven by interpreting DynamicCompiled over the oracle arg grid.

use std::collections::BTreeMap;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{CallExpression, Declaration, Expression, Statement};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};
use serde_json::{Map, Value};

use oj_stylex::errors::StylexError;
use oj_stylex::eval::value::EvalValue;
use oj_stylex::eval::{Evaluator, evaluate_stylex_create_arg, unwrap_parens, validate_create_arg};
use oj_stylex::imports::StylexNamedImport;
use oj_stylex::jsrt::js_number_to_string;
use oj_stylex::module_resolution::{FsProvider, StdFs};
use oj_stylex::options::{CompilerOptions, ResolvedOptions};
use oj_stylex::rules::StylexRule;
use oj_stylex::shared::create::{CreateContext, compile_namespaces};
use oj_stylex::shared::dynamic::{
    ClassPart, DynamicCompiled, compile_dynamic_namespace, inherit_rules,
};
use oj_stylex::state::{CompileState, line_of_offset};
use oj_stylex::transform::visitor::RealCallables;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pin {
    name: String,
    source: String,
    filename: String,
    options: Value,
    export_name: String,
    #[serde(default)]
    skip_invoke: bool,
    #[serde(default)]
    expected_error: Option<ExpectedError>,
    #[serde(default)]
    expected_rules: Option<Vec<ExpectedRule>>,
    #[serde(default)]
    namespaces: Option<Value>,
}

#[derive(serde::Deserialize)]
struct ExpectedError {
    message: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedRule {
    class_name: String,
    ltr: String,
    rtl: Option<String>,
    priority: f64,
}

struct CompiledModule {
    /// Full metadata rule list: keyframes → create rules → @property.
    rules: Vec<StylexRule>,
    /// namespace → compiled object (fns namespaces hold their placeholders).
    compiled: Vec<(String, EvalValue)>,
    dynamic: Vec<(String, DynamicCompiled)>,
}

fn find_create_call<'a>(
    state: &CompileState<'_>,
    expr: &'a Expression<'a>,
) -> Option<&'a CallExpression<'a>> {
    let Expression::CallExpression(call) = unwrap_parens(expr) else {
        return None;
    };
    match unwrap_parens(&call.callee) {
        Expression::Identifier(id)
            if state.imports.named_binding(&id.name) == Some(StylexNamedImport::Create) =>
        {
            Some(call)
        }
        Expression::StaticMemberExpression(member) if member.property.name == "create" => {
            match &member.object {
                Expression::Identifier(object)
                    if state.imports.is_stylex_namespace(&object.name) =>
                {
                    Some(call)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn compile_module(
    source: &str,
    filename: &str,
    options: &ResolvedOptions,
    export_name: &str,
) -> Result<CompiledModule, StylexError> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(filename).unwrap_or_else(|_| SourceType::tsx());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    assert!(
        !parsed.fatal_error && !parsed.diagnostics.has_errors(),
        "parse failed for {filename}"
    );
    let mut state = CompileState::build(
        &parsed.program,
        options,
        Some(filename.to_string()),
        "/fake".to_string(),
    )?;

    let mut calls: Vec<(Option<String>, &CallExpression<'_>)> = Vec::new();
    for statement in &parsed.program.body {
        let declaration = match statement {
            Statement::VariableDeclaration(decl) => Some(&**decl),
            Statement::ExportDeclaration(export) => match &export.declaration {
                Declaration::VariableDeclaration(decl) => Some(&**decl),
                _ => None,
            },
            Statement::ExportDefaultDeclaration(export) => {
                if let Some(expr) = export.declaration.as_expression()
                    && let Some(call) = find_create_call(&state, expr)
                {
                    calls.push((None, call));
                }
                None
            }
            _ => None,
        };
        let Some(declaration) = declaration else {
            continue;
        };
        for declarator in &declaration.declarations {
            if let Some(init) = &declarator.init
                && let Some(call) = find_create_call(&state, init)
            {
                let var_name = declarator
                    .id
                    .get_binding_identifier()
                    .map(|id| id.name.to_string());
                calls.push((var_name, call));
            }
        }
    }
    assert_eq!(calls.len(), 1, "dynamic pins hold exactly one create call");
    let (var_name, call) = calls.remove(0);
    assert_eq!(
        var_name.as_deref().unwrap_or("default"),
        export_name,
        "pin export name matches the bound variable"
    );

    let object = validate_create_arg(call)?;
    let arg = {
        let mut evaluator = Evaluator::for_create(&mut state, &StdFs, &RealCallables);
        evaluator.allow_dynamic_namespaces();
        evaluate_stylex_create_arg(&mut evaluator, object)?
    };
    let mut namespace_lines: BTreeMap<String, u32> = BTreeMap::new();
    for (key, offset) in &arg.key_spans {
        namespace_lines.insert(key.clone(), line_of_offset(source, *offset));
    }
    let file_package = StdFs
        .nearest_package(Path::new(filename))
        .map(|(name, dir)| (name, dir.to_string_lossy().into_owned()));
    let ctx = CreateContext {
        options,
        filename: Some(filename.to_string()),
        cwd: "/fake".to_string(),
        var_name,
        namespace_lines: Some(namespace_lines),
        file_package,
        cwd_package_name: None,
    };
    let out = compile_namespaces(&arg.namespaces, &ctx, !arg.fns.is_empty())?;

    let inherit = inherit_rules(&arg.fns);
    let mut rules: Vec<StylexRule> = std::mem::take(&mut state.other_injected_rules);
    rules.extend(out.rules.iter().cloned());
    rules.extend(inherit.iter().cloned());

    let mut dynamic = Vec::new();
    for (ns, fn_def) in &arg.fns {
        let Some(EvalValue::Obj(compiled_ns)) = out.compiled.get(ns) else {
            panic!("fns namespace {ns} missing from compiled output");
        };
        let empty = Vec::new();
        let class_paths = out
            .class_paths
            .iter()
            .find(|(name, _)| name == ns)
            .map(|(_, paths)| paths)
            .unwrap_or(&empty);
        dynamic.push((
            ns.clone(),
            compile_dynamic_namespace(compiled_ns, class_paths, fn_def, &rules, options),
        ));
    }

    Ok(CompiledModule {
        rules,
        compiled: out
            .compiled
            .entries()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        dynamic,
    })
}

// A small JS interpreter: exactly the expression surface the pin matrix uses,
// with V8-faithful coercions (the oracle grid is the arbiter).

#[derive(Debug, Clone, PartialEq)]
enum Rt {
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Fn,
}

type RtResult = Result<Rt, String>;

struct Interp<'e> {
    env: &'e [(String, Rt)],
}

impl Interp<'_> {
    fn lookup(&self, name: &str) -> Rt {
        if let Some((_, v)) = self.env.iter().find(|(n, _)| n == name) {
            return v.clone();
        }
        match name {
            "undefined" => Rt::Undefined,
            "NaN" => Rt::Num(f64::NAN),
            "Infinity" => Rt::Num(f64::INFINITY),
            other => panic!("interpreter: unbound identifier `{other}`"),
        }
    }

    fn eval(&self, expr: &Expression<'_>) -> RtResult {
        match expr {
            Expression::ParenthesizedExpression(e) => self.eval(&e.expression),
            Expression::NullLiteral(_) => Ok(Rt::Null),
            Expression::BooleanLiteral(lit) => Ok(Rt::Bool(lit.value)),
            Expression::NumericLiteral(lit) => Ok(Rt::Num(lit.value)),
            Expression::StringLiteral(lit) => Ok(Rt::Str(lit.value.to_string())),
            Expression::Identifier(id) => Ok(self.lookup(&id.name)),
            Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => {
                Ok(Rt::Fn)
            }
            Expression::TemplateLiteral(tpl) => {
                let mut out = String::new();
                for (i, quasi) in tpl.quasis.iter().enumerate() {
                    out.push_str(quasi.value.cooked.as_deref().unwrap_or("undefined"));
                    if let Some(part) = tpl.expressions.get(i) {
                        out.push_str(&to_string(&self.eval(part)?));
                    }
                }
                Ok(Rt::Str(out))
            }
            Expression::ConditionalExpression(cond) => {
                if truthy(&self.eval(&cond.test)?) {
                    self.eval(&cond.consequent)
                } else {
                    self.eval(&cond.alternate)
                }
            }
            Expression::LogicalExpression(logical) => {
                let left = self.eval(&logical.left)?;
                match logical.operator.as_str() {
                    "&&" => {
                        if truthy(&left) {
                            self.eval(&logical.right)
                        } else {
                            Ok(left)
                        }
                    }
                    "||" => {
                        if truthy(&left) {
                            Ok(left)
                        } else {
                            self.eval(&logical.right)
                        }
                    }
                    "??" => {
                        if is_nullish(&left) {
                            self.eval(&logical.right)
                        } else {
                            Ok(left)
                        }
                    }
                    other => panic!("interpreter: logical `{other}`"),
                }
            }
            Expression::UnaryExpression(unary) => {
                if unary.operator.as_str() == "void" {
                    self.eval(&unary.argument)?;
                    return Ok(Rt::Undefined);
                }
                let arg = self.eval(&unary.argument)?;
                Ok(match unary.operator.as_str() {
                    "-" => Rt::Num(-to_number(&arg)),
                    "+" => Rt::Num(to_number(&arg)),
                    "!" => Rt::Bool(!truthy(&arg)),
                    "typeof" => Rt::Str(
                        match arg {
                            Rt::Undefined => "undefined",
                            Rt::Null => "object",
                            Rt::Bool(_) => "boolean",
                            Rt::Num(_) => "number",
                            Rt::Str(_) => "string",
                            Rt::Fn => "function",
                        }
                        .to_string(),
                    ),
                    other => panic!("interpreter: unary `{other}`"),
                })
            }
            Expression::BinaryExpression(binary) => {
                let left = self.eval(&binary.left)?;
                let right = self.eval(&binary.right)?;
                Ok(match binary.operator.as_str() {
                    "+" => js_add(&left, &right),
                    "-" => Rt::Num(to_number(&left) - to_number(&right)),
                    "*" => Rt::Num(to_number(&left) * to_number(&right)),
                    "/" => Rt::Num(to_number(&left) / to_number(&right)),
                    "%" => Rt::Num(to_number(&left) % to_number(&right)),
                    "**" => Rt::Num(to_number(&left).powf(to_number(&right))),
                    "==" => Rt::Bool(loose_eq(&left, &right)),
                    "!=" => Rt::Bool(!loose_eq(&left, &right)),
                    "===" => Rt::Bool(strict_eq(&left, &right)),
                    "!==" => Rt::Bool(!strict_eq(&left, &right)),
                    "<" | ">" | "<=" | ">=" => js_compare(binary.operator.as_str(), &left, &right),
                    other => panic!("interpreter: binary `{other}`"),
                })
            }
            Expression::StaticMemberExpression(member) => {
                self.member(&self.eval(&member.object)?, &member.property.name)
            }
            Expression::ComputedMemberExpression(member) => {
                let key = to_string(&self.eval(&member.expression)?);
                self.member(&self.eval(&member.object)?, &key)
            }
            other => panic!("interpreter: unsupported expression {other:?}"),
        }
    }

    fn member(&self, object: &Rt, key: &str) -> RtResult {
        match object {
            Rt::Null => Err(format!("Cannot read properties of null (reading '{key}')")),
            Rt::Undefined => Err(format!(
                "Cannot read properties of undefined (reading '{key}')"
            )),
            Rt::Str(s) if key == "length" => Ok(Rt::Num(s.encode_utf16().count() as f64)),
            _ => Ok(Rt::Undefined),
        }
    }
}

fn truthy(v: &Rt) -> bool {
    match v {
        Rt::Undefined | Rt::Null => false,
        Rt::Bool(b) => *b,
        Rt::Num(n) => *n != 0.0 && !n.is_nan(),
        Rt::Str(s) => !s.is_empty(),
        Rt::Fn => true,
    }
}

fn is_nullish(v: &Rt) -> bool {
    matches!(v, Rt::Undefined | Rt::Null)
}

fn to_number(v: &Rt) -> f64 {
    match v {
        Rt::Undefined => f64::NAN,
        Rt::Null => 0.0,
        Rt::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Rt::Num(n) => *n,
        Rt::Str(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                0.0
            } else {
                trimmed.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        Rt::Fn => f64::NAN,
    }
}

fn to_string(v: &Rt) -> String {
    match v {
        Rt::Undefined => "undefined".to_string(),
        Rt::Null => "null".to_string(),
        Rt::Bool(b) => b.to_string(),
        Rt::Num(n) => js_number_to_string(*n),
        Rt::Str(s) => s.clone(),
        Rt::Fn => panic!("interpreter: ToString on a function"),
    }
}

fn js_add(l: &Rt, r: &Rt) -> Rt {
    if matches!(l, Rt::Str(_)) || matches!(r, Rt::Str(_)) {
        Rt::Str(format!("{}{}", to_string(l), to_string(r)))
    } else {
        Rt::Num(to_number(l) + to_number(r))
    }
}

fn loose_eq(l: &Rt, r: &Rt) -> bool {
    match (l, r) {
        (Rt::Null | Rt::Undefined, Rt::Null | Rt::Undefined) => true,
        (Rt::Null | Rt::Undefined, _) | (_, Rt::Null | Rt::Undefined) => false,
        (Rt::Str(a), Rt::Str(b)) => a == b,
        _ => {
            let (a, b) = (to_number(l), to_number(r));
            a == b
        }
    }
}

fn strict_eq(l: &Rt, r: &Rt) -> bool {
    match (l, r) {
        (Rt::Num(a), Rt::Num(b)) => a == b,
        _ => l == r,
    }
}

fn js_compare(op: &str, l: &Rt, r: &Rt) -> Rt {
    if let (Rt::Str(a), Rt::Str(b)) = (l, r) {
        let ord = a
            .encode_utf16()
            .collect::<Vec<_>>()
            .cmp(&b.encode_utf16().collect::<Vec<_>>());
        return Rt::Bool(match op {
            "<" => ord.is_lt(),
            ">" => ord.is_gt(),
            "<=" => ord.is_le(),
            _ => ord.is_ge(),
        });
    }
    let (a, b) = (to_number(l), to_number(r));
    if a.is_nan() || b.is_nan() {
        return Rt::Bool(false);
    }
    Rt::Bool(match op {
        "<" => a < b,
        ">" => a > b,
        "<=" => a <= b,
        _ => a >= b,
    })
}

// ---------------------------------------------------------------------------
// DynamicCompiled interpretation over one argument tuple (deepSerialize shape)

fn serialize_rt(v: &Rt) -> Value {
    match v {
        Rt::Undefined => serde_json::json!({ "__undefined__": true }),
        Rt::Null => Value::Null,
        Rt::Bool(b) => Value::Bool(*b),
        Rt::Num(n) => serialize_number(*n),
        Rt::Str(s) => Value::String(s.clone()),
        Rt::Fn => serde_json::json!({ "__unserializable__": "function" }),
    }
}

fn serialize_number(n: f64) -> Value {
    if n.is_finite() && !(n == 0.0 && n.is_sign_negative()) {
        if n.fract() == 0.0 && n.abs() < 9e15 {
            if n < 0.0 {
                Value::from(n as i64)
            } else {
                Value::from(n as u64)
            }
        } else {
            Value::from(n)
        }
    } else {
        let repr = if n == 0.0 {
            "-0".to_string()
        } else {
            js_number_to_string(n)
        };
        serde_json::json!({ "__number__": repr })
    }
}

fn eval_value_to_artifact(v: &EvalValue) -> Value {
    // Compiled namespaces hold strings, nulls, and the $$css tag only.
    v.to_json()
}

struct ExprTable<'a> {
    exprs: Vec<(Span, Expression<'a>)>,
}

impl<'a> ExprTable<'a> {
    fn build(
        allocator: &'a Allocator,
        source: &str,
        compiled: &DynamicCompiled,
        slices: &'a mut Vec<String>,
    ) -> Self {
        let mut spans: Vec<Span> = Vec::new();
        for (_, parts) in &compiled.conditional_props {
            for part in parts {
                if let ClassPart::Guarded { expr, .. } = part
                    && !spans.contains(expr)
                {
                    spans.push(*expr);
                }
            }
        }
        for var in &compiled.inline_vars {
            if !spans.contains(&var.expr) {
                spans.push(var.expr);
            }
        }
        for span in &spans {
            slices.push(source[span.start as usize..span.end as usize].to_string());
        }
        let mut exprs = Vec::new();
        for (span, text) in spans.iter().zip(slices.iter()) {
            let parsed = Parser::new(allocator, text, SourceType::ts())
                .parse_expression()
                .unwrap_or_else(|_| panic!("re-parse of `{text}` failed"));
            exprs.push((*span, parsed));
        }
        ExprTable { exprs }
    }

    fn get(&self, span: Span) -> &Expression<'a> {
        &self
            .exprs
            .iter()
            .find(|(s, _)| *s == span)
            .expect("span registered")
            .1
    }
}

fn invoke(compiled: &DynamicCompiled, table: &ExprTable<'_>, args: &[Rt]) -> Result<Value, String> {
    let env: Vec<(String, Rt)> = compiled
        .params
        .iter()
        .cloned()
        .zip(args.iter().cloned())
        .collect();
    let interp = Interp { env: &env };
    let css_tag = eval_value_to_artifact(&compiled.css_tag);

    let mut elements: Vec<Value> = Vec::new();
    if !compiled.static_props.is_empty() {
        let mut obj = Map::new();
        for (key, segments) in &compiled.static_props {
            obj.insert(key.clone(), Value::String(segments.concat()));
        }
        obj.insert("$$css".to_string(), css_tag.clone());
        elements.push(Value::Object(obj));
    }
    if !compiled.conditional_props.is_empty() {
        let mut obj = Map::new();
        for (key, parts) in &compiled.conditional_props {
            let mut acc: Option<Rt> = None;
            for part in parts {
                let value = match part {
                    ClassPart::Lit(lit) => Rt::Str(lit.clone()),
                    ClassPart::Guarded { lit, expr } => {
                        let v = interp.eval(table.get(*expr))?;
                        if is_nullish(&v) {
                            v
                        } else {
                            Rt::Str(lit.clone())
                        }
                    }
                };
                acc = Some(match acc {
                    None => value,
                    Some(prev) => js_add(&prev, &value),
                });
            }
            let joined = acc.expect("conditional props hold at least one part");
            obj.insert(key.clone(), serialize_rt(&joined));
        }
        obj.insert("$$css".to_string(), css_tag.clone());
        elements.push(Value::Object(obj));
    }

    let mut vars = Map::new();
    for var in &compiled.inline_vars {
        let v = interp.eval(table.get(var.expr))?;
        let wrapped = if var.unit.is_empty() {
            if is_nullish(&v) {
                interp.lookup("undefined")
            } else {
                v
            }
        } else if let Rt::Num(n) = v {
            Rt::Str(format!("{}{}", js_number_to_string(n), var.unit))
        } else if is_nullish(&v) {
            interp.lookup("undefined")
        } else {
            v
        };
        vars.insert(var.var_name.clone(), serialize_rt(&wrapped));
    }
    let vars = Value::Object(vars);

    if elements.is_empty() {
        Ok(vars)
    } else {
        elements.push(vars);
        Ok(Value::Array(elements))
    }
}

fn parse_arg_key(key: &str) -> Vec<Rt> {
    if key.is_empty() {
        return Vec::new();
    }
    key.split(',')
        .map(|token| match token {
            "undefined" => Rt::Undefined,
            "null" => Rt::Null,
            other => match serde_json::from_str::<Value>(other).expect("grid token") {
                Value::Number(n) => Rt::Num(n.as_f64().expect("finite grid number")),
                Value::String(s) => Rt::Str(s),
                Value::Bool(b) => Rt::Bool(b),
                Value::Null => Rt::Null,
                other => panic!("unexpected grid token {other:?}"),
            },
        })
        .collect()
}

// ---------------------------------------------------------------------------

fn assert_rules(label: &str, actual: &[StylexRule], expected: &[ExpectedRule]) {
    assert_eq!(actual.len(), expected.len(), "{label}: rule count");
    for (rule, expected) in actual.iter().zip(expected) {
        assert_eq!(&*rule.class_name, expected.class_name, "{label}: className");
        assert_eq!(
            &*rule.ltr, expected.ltr,
            "{label}: ltr of {}",
            rule.class_name
        );
        assert_eq!(
            rule.rtl.as_deref(),
            expected.rtl.as_deref(),
            "{label}: rtl of {}",
            rule.class_name
        );
        assert!(
            rule.priority == expected.priority,
            "{label}: priority of {} ({} != {})",
            rule.class_name,
            rule.priority,
            expected.priority
        );
    }
}

#[test]
fn oracle_dynamic_pins() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/pins/dynamic/dynamic.json"
    ))
    .expect("run crates/stylex/conformance/src/gen-pins-dynamic.mjs to generate pins");
    let pins: Vec<Pin> = serde_json::from_str(&raw).unwrap();
    assert!(
        pins.len() >= 80,
        "suspiciously small pin set: {}",
        pins.len()
    );

    let mut error_pins = 0;
    let mut fn_namespaces = 0;
    let mut invocations_checked = 0usize;
    for pin in &pins {
        let label = &pin.name;
        let options = CompilerOptions::from_json(&pin.options)
            .unwrap_or_else(|e| panic!("{label}: options parse: {e}"))
            .resolve()
            .unwrap_or_else(|e| panic!("{label}: options resolve: {e}"));
        let result = compile_module(&pin.source, &pin.filename, &options, &pin.export_name);

        if let Some(expected) = &pin.expected_error {
            error_pins += 1;
            let err = match result {
                Err(err) => err,
                Ok(_) => panic!("{label}: expected error {:?}", expected.message),
            };
            assert_eq!(
                err.message.lines().next().unwrap_or_default(),
                expected.message,
                "{label}: error message"
            );
            continue;
        }

        let output = result.unwrap_or_else(|e| panic!("{label}: unexpected error: {e}"));
        assert_rules(
            label,
            &output.rules,
            pin.expected_rules.as_ref().expect("rules pin"),
        );

        let expected_namespaces = pin
            .namespaces
            .as_ref()
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("{label}: namespaces pin"));
        let actual_keys: Vec<&str> = output.compiled.iter().map(|(k, _)| k.as_str()).collect();
        let expected_keys: Vec<&str> = expected_namespaces.keys().map(String::as_str).collect();
        assert_eq!(actual_keys, expected_keys, "{label}: namespace set");

        for (ns, expected) in expected_namespaces {
            let is_fn = expected.get("kind").and_then(Value::as_str) == Some("function");
            let dynamic = output.dynamic.iter().find(|(name, _)| name == ns);
            if !is_fn {
                assert!(dynamic.is_none(), "{label}/{ns}: unexpectedly dynamic");
                let (_, compiled) = output
                    .compiled
                    .iter()
                    .find(|(name, _)| name == ns)
                    .expect("namespace present");
                assert_eq!(
                    &eval_value_to_artifact(compiled),
                    expected,
                    "{label}/{ns}: static namespace"
                );
                continue;
            }
            let (_, compiled) = dynamic.unwrap_or_else(|| panic!("{label}/{ns}: not dynamic"));
            fn_namespaces += 1;
            let param_count = expected
                .get("paramCount")
                .and_then(Value::as_u64)
                .expect("paramCount");
            assert_eq!(
                compiled.params.len() as u64,
                param_count,
                "{label}/{ns}: param count"
            );
            if pin.skip_invoke {
                continue;
            }
            let invocations = expected
                .get("invocations")
                .and_then(Value::as_object)
                .expect("invocations");
            let allocator = Allocator::default();
            let mut slices = Vec::new();
            let table = ExprTable::build(&allocator, &pin.source, compiled, &mut slices);
            for (arg_key, expected_result) in invocations {
                let args = parse_arg_key(arg_key);
                let actual = match invoke(compiled, &table, &args) {
                    Ok(value) => value,
                    Err(message) => serde_json::json!({ "__threw__": message }),
                };
                assert_eq!(
                    &actual, expected_result,
                    "{label}/{ns}: invocation ({arg_key})"
                );
                invocations_checked += 1;
            }
        }
    }
    assert!(
        error_pins >= 10,
        "suspiciously few error pins: {error_pins}"
    );
    assert!(
        fn_namespaces >= 60,
        "suspiciously few fn namespaces: {fn_namespaces}"
    );
    assert!(
        invocations_checked >= 1000,
        "suspiciously few invocations: {invocations_checked}"
    );
}

/// The transform path keeps rejecting arrow namespaces until the integrator
/// flips `allow_dynamic_namespaces` and prints the arrows.
#[test]
fn dynamic_namespaces_stay_rejected_by_default() {
    let source = "import * as stylex from '@stylexjs/stylex';\nexport const styles = stylex.create({ a: (w) => ({ width: w }) });\n";
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::tsx()).parse();
    let options = ResolvedOptions::default();
    let mut state = CompileState::build(
        &parsed.program,
        &options,
        Some("/fake/root/pins.ts".to_string()),
        "/fake".to_string(),
    )
    .unwrap();
    let Statement::ExportDeclaration(export) = &parsed.program.body[1] else {
        panic!("expected export declaration");
    };
    let Declaration::VariableDeclaration(decl) = &export.declaration else {
        panic!("expected variable declaration");
    };
    let init = decl.declarations[0].init.as_ref().unwrap();
    let call = find_create_call(&state, init).expect("create call");
    let object = validate_create_arg(call).unwrap();
    let mut evaluator = Evaluator::for_create(&mut state, &StdFs, &RealCallables);
    let err = evaluate_stylex_create_arg(&mut evaluator, object).unwrap_err();
    assert!(
        err.message.contains("dynamic styles"),
        "default path must stay a hard error: {}",
        err.message
    );
}
