use rustc_hash::FxHashSet;
use tsrs_ast::{Kind, Node};
use tsrs_checker::{Checker, ObjectFlags, SignatureKind, Type, TypeFlags};
use tsrs_core::{P, json::Value};

use crate::protocol::{Fix, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};

pub struct PromiseFunctionAsync {
    allow_any: bool,
    allowed_promise_names: FxHashSet<String>,
    check_arrow_functions: bool,
    check_function_declarations: bool,
    check_function_expressions: bool,
    check_method_declarations: bool,
}

fn build_missing_async_message() -> RuleMessage {
    RuleMessage {
        id: "missingAsync".into(),
        description: "Functions that return promises must be async.".into(),
        help: String::new(),
    }
}

fn build_missing_async_hybrid_return_message() -> RuleMessage {
    RuleMessage {
        id: "missingAsyncHybridReturn".into(),
        description: "Functions that return promises must be async.".into(),
        help: "Consider adding an explicit return type annotation if the function is intended to return a union of promise and non-promise types.".into(),
    }
}

fn build_missing_async_hybrid_return_suggestion_message() -> RuleMessage {
    RuleMessage {
        id: "missingAsyncHybridReturnSuggestion".into(),
        description: "Add `async` keyword to the function.".into(),
        help: String::new(),
    }
}

impl PromiseFunctionAsync {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("promise-function-async: options must be an object".into()),
        };
        let boolean = |name: &str| match object.and_then(|object| object.get(name)) {
            None | Some(Value::Null) => Ok(true),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(format!("promise-function-async: {name} must be a boolean")),
        };
        let mut allowed_promise_names = match object
            .and_then(|object| object.get("allowedPromiseNames"))
        {
            None | Some(Value::Null) => FxHashSet::default(),
            Some(Value::Array(names)) => names
                .iter()
                .map(|name| match name {
                    Value::String(name) => Ok(name.clone()),
                    Value::Null => Ok(String::new()),
                    _ => {
                        Err("promise-function-async: allowedPromiseNames must contain strings"
                            .into())
                    }
                })
                .collect::<Result<_, String>>()?,
            _ => return Err("promise-function-async: allowedPromiseNames must be an array".into()),
        };
        allowed_promise_names.insert("Promise".into());
        Ok(Self {
            allow_any: boolean("allowAny")?,
            allowed_promise_names,
            check_arrow_functions: boolean("checkArrowFunctions")?,
            check_function_declarations: boolean("checkFunctionDeclarations")?,
            check_function_expressions: boolean("checkFunctionExpressions")?,
            check_method_declarations: boolean("checkMethodDeclarations")?,
        })
    }

    fn contains_all_types_by_name(
        &self,
        checker: &mut Checker,
        mut t: P<Type>,
        match_any_instead: bool,
    ) -> bool {
        if t.flags().intersects(TypeFlags::AnyOrUnknown) {
            return false;
        }
        if t.flags().intersects(TypeFlags::Object)
            && t.object_flags().intersects(ObjectFlags::Reference)
        {
            t = t.target().unwrap();
        }
        if t.symbol().is_some_and(|symbol| self.allowed_promise_names.contains(symbol.name())) {
            return true;
        }
        if t.flags().intersects(TypeFlags::UnionOrIntersection) {
            if match_any_instead {
                return t.types().iter().all(|&part| {
                    self.contains_all_types_by_name(checker, part, match_any_instead)
                });
            }
            return t
                .types()
                .iter()
                .any(|&part| self.contains_all_types_by_name(checker, part, match_any_instead));
        }
        if !t.object_flags().intersects(ObjectFlags::ClassOrInterface) {
            return false;
        }
        let bases = checker.get_base_types(t);
        if match_any_instead {
            return bases
                .iter()
                .any(|&base| self.contains_all_types_by_name(checker, base, match_any_instead));
        }
        !bases.is_empty()
            && bases
                .iter()
                .all(|&base| self.contains_all_types_by_name(checker, base, match_any_instead))
    }

    fn validate_node(&self, node: P<Node>, context: &mut RuleContext<'_>) {
        if node.modifiers().is_some_and(|modifiers| {
            modifiers.list.nodes().iter().any(|modifier| modifier.kind() == Kind::AsyncKeyword)
        }) || node.body().is_none()
        {
            return;
        }
        let t = context.checker.get_type_at_location(node);
        let signatures = context.checker.get_signatures_of_type(t, SignatureKind::Call);
        if signatures.is_empty() {
            return;
        }
        let has_explicit_return_type = node.type_node().is_some();
        let mut every_signature_returns_promise = true;
        for &signature in signatures {
            let return_type = context.checker.get_return_type_of_signature(signature);
            if !self.allow_any && return_type.flags().intersects(TypeFlags::AnyOrUnknown) {
                context.report_node(node, build_missing_async_message());
                return;
            }
            every_signature_returns_promise = every_signature_returns_promise
                && self.contains_all_types_by_name(
                    context.checker,
                    return_type,
                    has_explicit_return_type,
                );
        }
        if !every_signature_returns_promise {
            return;
        }
        let mut is_hybrid_return_type = false;
        if !has_explicit_return_type {
            for &signature in signatures {
                let return_type = context.checker.get_return_type_of_signature(signature);
                if return_type.flags().intersects(TypeFlags::Union)
                    && !return_type
                        .types()
                        .iter()
                        .all(|&part| self.contains_all_types_by_name(context.checker, part, true))
                {
                    is_hybrid_return_type = true;
                    break;
                }
            }
        }
        let source_file = context.source_file;
        let insert_async_fix = || {
            let mut pos = trim_node_range(source_file, node).pos;
            let mut text = " async ";
            if node.kind() == Kind::MethodDeclaration {
                pos = trim_node_range(source_file, node.name().unwrap()).pos;
            }
            if node.kind() == Kind::FunctionDeclaration
                && let Some(last_modifier) =
                    node.modifiers().and_then(|modifiers| modifiers.list.nodes().last().copied())
            {
                pos = last_modifier.end().expect_source("parsed lint modifier");
                text = " async";
            }
            Fix { text: text.into(), range: Range { pos, end: pos } }
        };
        if is_hybrid_return_type {
            context.report_node_with_suggestions(
                node,
                build_missing_async_hybrid_return_message(),
                || {
                    vec![Suggestion {
                        message: build_missing_async_hybrid_return_suggestion_message(),
                        fixes: vec![insert_async_fix()],
                    }]
                },
            );
        } else {
            context.report_node_with_fixes(node, build_missing_async_message(), || {
                vec![insert_async_fix()]
            });
        }
    }
}

impl Rule for PromiseFunctionAsync {
    fn listens_to(&self, listener: Listener) -> bool {
        match listener {
            Listener::Enter(Kind::ArrowFunction) => self.check_arrow_functions,
            Listener::Enter(Kind::FunctionDeclaration) => self.check_function_declarations,
            Listener::Enter(Kind::FunctionExpression) => self.check_function_expressions,
            Listener::Enter(Kind::MethodDeclaration) => self.check_method_declarations,
            _ => false,
        }
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        if node.kind() == Kind::MethodDeclaration
            && node.modifiers().is_some_and(|modifiers| {
                modifiers
                    .list
                    .nodes()
                    .iter()
                    .any(|modifier| modifier.kind() == Kind::AbstractKeyword)
            })
        {
            return;
        }
        self.validate_node(node, context);
    }
}
