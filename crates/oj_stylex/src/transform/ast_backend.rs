//! AST-mutation output backend (design-core.md §3a): replays the visitor's
//! structured replacement values onto the caller's `Program` via `AstBuilder`.

// Span invariant: synthesized nodes carry the replaced node's span (generated
// runtime imports an empty one — codegen skips those); clones keep their own.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use oxc_allocator::{Allocator, CloneIn, Vec as ArenaVec};
use oxc_ast::ast::{
    Argument, ArrayExpressionElement, ArrowFunctionBody, ArrowFunctionExpression,
    BindingIdentifier, BindingPattern, Comment, ComputedMemberExpression, Declaration, Expression,
    FormalParameter, FormalParameterKind, FormalParameters, IdentifierName, ImportDeclaration,
    ImportDeclarationSpecifier, ImportDefaultSpecifier, ImportNamespaceSpecifier,
    ImportOrExportKind, ImportSpecifier, JSXAttribute, JSXAttributeItem, JSXAttributeName,
    JSXAttributeValue, JSXElement, JSXOpeningElement, JSXSpreadAttribute, ModuleExportName,
    NumberBase, ObjectExpression, ObjectProperty, ObjectPropertyKind, Program, PropertyKey,
    PropertyKind, Statement, StringLiteral, VariableDeclaration, VariableDeclarationKind,
    VariableDeclarator,
};
use oxc_ast::builder::AstBuilder;
use oxc_ast_visit::{Visit, VisitMut, walk, walk_mut};
use oxc_span::{GetSpan, Span};
use oxc_syntax::operator::{BinaryOperator, UnaryOperator};

use crate::errors::{ErrorCode, StylexError};
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::jsrt::js_number_to_string;
use crate::rules::StylexRule;
use crate::shared::dynamic::{ClassPart, DynamicCompiled};
use crate::transform::js_out::{
    const_val_number, const_val_string, is_identifier_key, js_string_literal,
};

// ---------------------------------------------------------------------------

// Plan: owned, span-keyed mutation descriptors the visitor records beside its
// splice edits; applying them to the program matches splicing the text.

#[derive(Debug, Clone)]
pub struct DynamicEntry {
    pub namespace: String,
    pub compiled: DynamicCompiled,
    /// Hoisted static-chunk const name; `None` prints the chunk inline.
    pub static_ident: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SynthExpr {
    /// `js_string_literal(text)` replacements (keyframes/positionTry names).
    Str(String),
    /// `print_parens`-style replacements (theming, markers, inlined props).
    ParenValue(EvalValue),
    /// Hoisted-create references.
    Ident(String),
    /// `print_create_parens(map, dynamic)` — compiled create objects.
    CreateObject {
        map: Arc<JsObjectMap>,
        dynamic: Vec<DynamicEntry>,
    },
    /// `table_text(entries, tests)` — the props/attrs/legacy bitmask lookup;
    /// leaves are merged objects for props/attrs, class strings for legacy.
    Table {
        entries: Vec<(u32, EvalValue)>,
        tests: Vec<Span>,
    },
    /// `{hoisted}.{property}` — a dynamic atom's rewritten callee.
    AtomCallee { hoisted: String, property: String },
}

#[derive(Debug, Clone)]
pub enum HoistValue {
    /// `print_static_chunk` — the shared dynamic static chunk.
    StaticChunk(DynamicCompiled),
    /// `print_create_parens` — non-program-level create result.
    CreateObject {
        map: Arc<JsObjectMap>,
        dynamic: Vec<DynamicEntry>,
    },
    /// A static atom props() could not merge, hoisted to module scope.
    ParenValue(EvalValue),
    /// The `{property: _v => [...]}` object a dynamic atom hoists.
    AtomDynamic {
        property: String,
        prop_key: String,
        class_name: String,
        var_name: String,
    },
}

/// One generated program-prologue statement, already in final body order
/// (unshift order resolved, block-hoist sort applied).
#[derive(Debug, Clone)]
pub enum PrologueStmt {
    NamespaceImport {
        local: String,
        source: String,
    },
    InjectImport {
        local: String,
        source: String,
        named: Option<String>,
    },
    InjectAlias {
        name: String,
        local: String,
    },
}

#[derive(Debug, Clone)]
pub enum SynthStmt {
    /// `const {name} = {value};`
    ConstDecl { name: String, value: HoistValue },
    /// The generated program-prologue block.
    Prologue(Vec<PrologueStmt>),
    /// One runtime-injection call: `{callee}({ltr, …});`
    InjectCall {
        callee: String,
        rule: Box<StylexRule>,
    },
    /// `import "{specifier}";` treeshake compensation.
    SideEffectImport {
        specifier: String,
        source_span: Span,
    },
}

#[derive(Debug, Clone)]
pub enum JsxOp {
    /// The spread/sx attribute becomes plain string attributes.
    Attrs(JsObjectMap),
    /// `{...({})}`
    SpreadEmptyObject,
    /// `{...({…}[lookup])}`
    SpreadTable {
        entries: Vec<(u32, EvalValue)>,
        tests: Vec<Span>,
    },
    /// `{...{local}.props({expr})}` — the sx bail path.
    SpreadProps { local: String, expr_span: Span },
}

#[derive(Debug, Clone)]
pub struct InsertOp {
    pub anchor: u32,
    /// Splice edit-slot order: ties at one anchor apply in ascending seq.
    pub seq: usize,
    /// Span stamped on the synthesized statement (see module invariant).
    pub span: Span,
    pub stmt: SynthStmt,
}

#[derive(Debug, Clone)]
pub struct RemoveOp {
    pub stmt_span: Span,
    pub decl_count: usize,
    pub indices: Vec<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct AstPlan {
    pub replace_exprs: Vec<(Span, SynthExpr)>,
    pub jsx_ops: Vec<(Span, JsxOp)>,
    pub inserts: Vec<InsertOp>,
    pub removes: Vec<RemoveOp>,
    /// `rewriteAliases`: (source literal span, its replacement value).
    pub import_sources: Vec<(Span, String)>,
}

impl AstPlan {
    pub fn is_empty(&self) -> bool {
        self.replace_exprs.is_empty()
            && self.jsx_ops.is_empty()
            && self.inserts.is_empty()
            && self.removes.is_empty()
            && self.import_sources.is_empty()
    }
}

fn internal(detail: String) -> StylexError {
    StylexError::new(ErrorCode::AstBackend, detail)
}

// ---------------------------------------------------------------------------

// Expression synthesis: mirrors the js_out.rs printers node-for-node, parens
// included, so reprints of both backends stay byte-equal.

struct Synth<'a> {
    b: AstBuilder<'a>,
    alloc: &'a Allocator,
    /// The replaced node's span, stamped on every synthesized node.
    span: Span,
}

impl<'a> Synth<'a> {
    fn s(&self, text: &str) -> &'a str {
        self.alloc.alloc_str(text)
    }

    fn ident(&self, name: &str) -> Expression<'a> {
        Expression::new_identifier(self.span, self.s(name), &self.b)
    }

    fn string(&self, value: &str) -> Expression<'a> {
        Expression::new_string_literal(self.span, self.s(value), None, &self.b)
    }

    fn paren(&self, inner: Expression<'a>) -> Expression<'a> {
        Expression::new_parenthesized_expression(self.span, inner, &self.b)
    }

    // Mirrors js_number_to_string output re-parsed: NaN/Infinity are
    // identifiers, a leading `-` is a unary minus, `-0` prints as `0`.
    fn number(&self, n: f64) -> Expression<'a> {
        let text = js_number_to_string(n);
        match text.as_str() {
            "NaN" | "Infinity" => self.ident(&text),
            t if t.starts_with('-') => {
                let inner = if &t[1..] == "Infinity" {
                    self.ident("Infinity")
                } else {
                    let value: f64 = t[1..].parse().unwrap_or(n.abs());
                    Expression::new_numeric_literal(
                        self.span,
                        value,
                        None,
                        NumberBase::Decimal,
                        &self.b,
                    )
                };
                Expression::new_unary_expression(
                    self.span,
                    UnaryOperator::UnaryNegation,
                    inner,
                    &self.b,
                )
            }
            t => {
                let value: f64 = t.parse().unwrap_or(n);
                Expression::new_numeric_literal(
                    self.span,
                    value,
                    None,
                    NumberBase::Decimal,
                    &self.b,
                )
            }
        }
    }

    fn prop_key(&self, key: &str) -> PropertyKey<'a> {
        if is_identifier_key(key) {
            PropertyKey::new_static_identifier(self.span, self.s(key), &self.b)
        } else {
            self.string_key(key)
        }
    }

    fn string_key(&self, key: &str) -> PropertyKey<'a> {
        PropertyKey::new_string_literal(self.span, self.s(key), None, &self.b)
    }

    fn prop(&self, key: PropertyKey<'a>, value: Expression<'a>) -> ObjectPropertyKind<'a> {
        ObjectPropertyKind::ObjectProperty(ObjectProperty::boxed(
            self.span,
            PropertyKind::Init,
            key,
            value,
            false,
            false,
            false,
            &self.b,
        ))
    }

    fn object(&self, props: Vec<ObjectPropertyKind<'a>>) -> Expression<'a> {
        let mut list = ArenaVec::with_capacity_in(props.len(), &self.b);
        list.extend(props);
        Expression::ObjectExpression(ObjectExpression::boxed(self.span, list, &self.b))
    }

    /// Mirrors js_out::write_value (arrays print as index-keyed objects).
    fn value(&self, value: &EvalValue) -> Expression<'a> {
        match value {
            EvalValue::Null => Expression::new_null_literal(self.span, &self.b),
            EvalValue::Undefined => self.ident("undefined"),
            EvalValue::Bool(b) => Expression::new_boolean_literal(self.span, *b, &self.b),
            EvalValue::Num(n) => self.number(*n),
            EvalValue::Str(s) => self.string(s),
            EvalValue::Arr(items) => {
                let props = items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| self.prop(self.string_key(&i.to_string()), self.value(item)))
                    .collect();
                self.object(props)
            }
            EvalValue::Obj(map) => self.object_value(map),
        }
    }

    fn object_value(&self, map: &JsObjectMap) -> Expression<'a> {
        let props = map
            .entries()
            .map(|(key, value)| self.prop(self.prop_key(key), self.value(value)))
            .collect();
        self.object(props)
    }

    /// Mirrors js_out::print_static_chunk (`"$$css"` stays a quoted key).
    fn static_chunk(&self, compiled: &DynamicCompiled) -> Expression<'a> {
        let mut props: Vec<ObjectPropertyKind<'a>> = compiled
            .static_props
            .iter()
            .map(|(key, segments)| self.prop(self.prop_key(key), self.class_segments(segments)))
            .collect();
        props.push(self.prop(self.string_key("$$css"), self.value(&compiled.css_tag)));
        self.object(props)
    }

    /// The atoms transform builds both the key and the member with
    /// `t.identifier(property)`, so a non-identifier property prints raw.
    fn raw_member(&self, object: &str, property: &str) -> Expression<'a> {
        Expression::new_static_member_expression(
            self.span,
            self.ident(object),
            IdentifierName::new(self.span, self.s(property), &self.b),
            false,
            &self.b,
        )
    }

    /// Mirrors the `const _temp = {prop: _v => [...]}` a dynamic atom hoists.
    fn atom_dynamic(
        &self,
        property: &str,
        prop_key: &str,
        class_name: &str,
        var_name: &str,
    ) -> Expression<'a> {
        let not_null = || self.binary(BinaryOperator::Inequality, self.ident("_v"), self.null());
        let compiled = self.object(vec![
            self.prop(
                self.string_key(prop_key),
                self.cond(not_null(), self.string(class_name), self.ident("_v")),
            ),
            self.prop(self.string_key("$$css"), self.value(&EvalValue::Bool(true))),
        ]);
        let vars = self.object(vec![self.prop(
            self.string_key(var_name),
            self.cond(not_null(), self.ident("_v"), self.ident("undefined")),
        )]);
        let mut items = ArenaVec::with_capacity_in(2, &self.b);
        items.push(ArrayExpressionElement::from(compiled));
        items.push(ArrayExpressionElement::from(vars));
        let body = Expression::new_array_expression(self.span, items, &self.b);
        let arrow = self.arrow(&["_v".to_string()], body);
        self.object(vec![self.prop(
            PropertyKey::new_static_identifier(self.span, self.s(property), &self.b),
            arrow,
        )])
    }

    /// Mirrors js_out::write_class_segments: an unfolded `+` chain.
    fn class_segments(&self, segments: &[String]) -> Expression<'a> {
        let mut iter = segments.iter();
        let Some(first) = iter.next() else {
            return self.string("");
        };
        iter.fold(self.string(first), |acc, seg| {
            self.binary(BinaryOperator::Addition, acc, self.string(seg))
        })
    }

    fn binary(
        &self,
        op: BinaryOperator,
        left: Expression<'a>,
        right: Expression<'a>,
    ) -> Expression<'a> {
        Expression::new_binary_expression(self.span, left, op, right, &self.b)
    }

    fn cond(
        &self,
        test: Expression<'a>,
        consequent: Expression<'a>,
        alternate: Expression<'a>,
    ) -> Expression<'a> {
        Expression::new_conditional_expression(self.span, test, consequent, alternate, &self.b)
    }

    fn null(&self) -> Expression<'a> {
        Expression::new_null_literal(self.span, &self.b)
    }

    fn arrow(&self, params: &[String], body: Expression<'a>) -> Expression<'a> {
        let mut items = ArenaVec::with_capacity_in(params.len(), &self.b);
        for name in params {
            let pattern = BindingPattern::BindingIdentifier(oxc_allocator::Box::new_in(
                BindingIdentifier::new(self.span, self.s(name), &self.b),
                &self.b,
            ));
            items.push(FormalParameter::new(
                self.span,
                ArenaVec::new_in(&self.b),
                pattern,
                None,
                None,
                false,
                None,
                false,
                false,
                &self.b,
            ));
        }
        let params = FormalParameters::boxed(
            self.span,
            FormalParameterKind::ArrowFormalParameters,
            items,
            None,
            &self.b,
        );
        Expression::ArrowFunctionExpression(ArrowFunctionExpression::boxed(
            self.span,
            false,
            None,
            params,
            None,
            ArrowFunctionBody::from(body),
            &self.b,
        ))
    }

    /// Mirrors js_out::print_dynamic_arrow; `resolve` clones the original
    /// expression at a recorded span (pre-mutation, like the splice printer).
    fn dynamic_arrow(
        &self,
        compiled: &DynamicCompiled,
        static_ident: Option<&str>,
        resolve: &mut dyn FnMut(Span) -> Result<Expression<'a>, StylexError>,
    ) -> Result<Expression<'a>, StylexError> {
        let mut var_props: Vec<ObjectPropertyKind<'a>> = Vec::new();
        for var in &compiled.inline_vars {
            let value = if var.unit.is_empty() {
                // ({src}) != null ? ({src}) : undefined
                let test = self.binary(
                    BinaryOperator::Inequality,
                    self.paren(resolve(var.expr)?),
                    self.null(),
                );
                self.cond(
                    test,
                    self.paren(resolve(var.expr)?),
                    self.ident("undefined"),
                )
            } else {
                // ((val) => typeof val === "number" ? val + {unit}
                //   : val != null ? val : undefined)(({src}))
                let typeof_test = self.binary(
                    BinaryOperator::StrictEquality,
                    Expression::new_unary_expression(
                        self.span,
                        UnaryOperator::Typeof,
                        self.ident("val"),
                        &self.b,
                    ),
                    self.string("number"),
                );
                let with_unit = self.binary(
                    BinaryOperator::Addition,
                    self.ident("val"),
                    self.string(&var.unit),
                );
                let nullish = self.cond(
                    self.binary(BinaryOperator::Inequality, self.ident("val"), self.null()),
                    self.ident("val"),
                    self.ident("undefined"),
                );
                let body = self.cond(typeof_test, with_unit, nullish);
                let callee = self.paren(self.arrow(&["val".to_string()], body));
                let mut args = ArenaVec::with_capacity_in(1, &self.b);
                args.push(Argument::from(self.paren(resolve(var.expr)?)));
                Expression::CallExpression(oxc_ast::ast::CallExpression::boxed(
                    self.span, callee, None, args, false, &self.b,
                ))
            };
            var_props.push(self.prop(self.string_key(&var.var_name), value));
        }
        let vars_obj = self.object(var_props);

        if !compiled.has_container() {
            return Ok(self.arrow(&compiled.params, self.paren(vars_obj)));
        }

        let mut items: Vec<Expression<'a>> = Vec::new();
        if !compiled.static_props.is_empty() {
            items.push(match static_ident {
                Some(name) => self.ident(name),
                None => self.static_chunk(compiled),
            });
        }
        if !compiled.conditional_props.is_empty() {
            let mut props: Vec<ObjectPropertyKind<'a>> = Vec::new();
            for (key, parts) in &compiled.conditional_props {
                let mut rendered: Vec<Expression<'a>> = Vec::new();
                for part in parts {
                    rendered.push(match part {
                        ClassPart::Lit(lit) => self.string(lit),
                        ClassPart::Guarded { lit, expr } => {
                            // (({src}) != null ? {lit} : ({src}))
                            let test = self.binary(
                                BinaryOperator::Inequality,
                                self.paren(resolve(*expr)?),
                                self.null(),
                            );
                            self.paren(self.cond(
                                test,
                                self.string(lit),
                                self.paren(resolve(*expr)?),
                            ))
                        }
                    });
                }
                let value = match rendered
                    .into_iter()
                    .reduce(|left, right| self.binary(BinaryOperator::Addition, left, right))
                {
                    Some(joined) => joined,
                    None => self.string(""),
                };
                props.push(self.prop(self.prop_key(key), value));
            }
            // parity: the conditional chunk's $$css key is an identifier.
            props.push(self.prop(
                PropertyKey::new_static_identifier(self.span, self.s("$$css"), &self.b),
                self.value(&compiled.css_tag),
            ));
            items.push(self.object(props));
        }
        items.push(vars_obj);
        let mut elements = ArenaVec::with_capacity_in(items.len(), &self.b);
        for item in items {
            elements.push(oxc_ast::ast::ArrayExpressionElement::from(item));
        }
        let array = Expression::ArrayExpression(oxc_ast::ast::ArrayExpression::boxed(
            self.span, elements, &self.b,
        ));
        Ok(self.arrow(&compiled.params, array))
    }

    /// Mirrors visitor::print_create_parens (the fns rewrite only fires on
    /// object-shaped values).
    fn create_object(
        &self,
        map: &JsObjectMap,
        dynamic: &[DynamicEntry],
        resolve: &mut dyn FnMut(Span) -> Result<Expression<'a>, StylexError>,
    ) -> Result<Expression<'a>, StylexError> {
        if dynamic.is_empty() {
            return Ok(self.paren(self.object_value(map)));
        }
        let mut props: Vec<ObjectPropertyKind<'a>> = Vec::new();
        for (key, value) in map.entries() {
            let entry = dynamic.iter().find(|e| e.namespace == key);
            let built = match entry {
                Some(entry) if matches!(value, EvalValue::Obj(_)) => {
                    self.dynamic_arrow(&entry.compiled, entry.static_ident.as_deref(), resolve)?
                }
                _ => self.value(value),
            };
            props.push(self.prop(self.prop_key(key), built));
        }
        Ok(self.paren(self.object(props)))
    }

    /// Mirrors visitor::table_text: `({k: {…}, …}[!!(t0) << n-1 | …])`.
    fn table(&self, entries: &[(u32, EvalValue)], tests: Vec<Expression<'a>>) -> Expression<'a> {
        let props = entries
            .iter()
            .map(|(key, leaf)| {
                self.prop(
                    PropertyKey::new_numeric_literal(
                        self.span,
                        f64::from(*key),
                        None,
                        NumberBase::Decimal,
                        &self.b,
                    ),
                    self.value(leaf),
                )
            })
            .collect();
        let object = self.object(props);
        let n = tests.len();
        let lookup = tests
            .into_iter()
            .enumerate()
            .map(|(i, test)| {
                let bang = Expression::new_unary_expression(
                    self.span,
                    UnaryOperator::LogicalNot,
                    Expression::new_unary_expression(
                        self.span,
                        UnaryOperator::LogicalNot,
                        self.paren(test),
                        &self.b,
                    ),
                    &self.b,
                );
                self.binary(
                    BinaryOperator::ShiftLeft,
                    bang,
                    self.number((n - 1 - i) as f64),
                )
            })
            .reduce(|left, right| self.binary(BinaryOperator::BitwiseOR, left, right))
            .expect("plan_merge tables carry at least one conditional");
        self.paren(Expression::ComputedMemberExpression(
            ComputedMemberExpression::boxed(self.span, object, lookup, false, &self.b),
        ))
    }

    /// Mirrors visitor::jsx_attrs_text (`&quot;`-escaped string attributes).
    fn jsx_attrs(&self, map: &JsObjectMap) -> Vec<JSXAttributeItem<'a>> {
        map.entries()
            .map(|(key, value)| {
                let text = match value {
                    EvalValue::Str(s) => s.replace('"', "&quot;"),
                    _ => "[object Object]".to_string(),
                };
                JSXAttributeItem::Attribute(JSXAttribute::boxed(
                    self.span,
                    JSXAttributeName::new_identifier(self.span, self.s(key), &self.b),
                    Some(JSXAttributeValue::new_string_literal(
                        self.span,
                        self.s(&text),
                        None,
                        &self.b,
                    )),
                    &self.b,
                ))
            })
            .collect()
    }

    fn spread_attr(&self, argument: Expression<'a>) -> JSXAttributeItem<'a> {
        JSXAttributeItem::SpreadAttribute(JSXSpreadAttribute::boxed(self.span, argument, &self.b))
    }

    fn props_call(&self, local: &str, argument: Expression<'a>) -> Expression<'a> {
        let callee = Expression::new_static_member_expression(
            self.span,
            self.ident(local),
            IdentifierName::new(self.span, self.s("props"), &self.b),
            false,
            &self.b,
        );
        let mut args = ArenaVec::with_capacity_in(1, &self.b);
        args.push(Argument::from(argument));
        Expression::CallExpression(oxc_ast::ast::CallExpression::boxed(
            self.span, callee, None, args, false, &self.b,
        ))
    }

    fn const_stmt(&self, name: &str, init: Expression<'a>) -> Statement<'a> {
        let id = BindingPattern::BindingIdentifier(oxc_allocator::Box::new_in(
            BindingIdentifier::new(self.span, self.s(name), &self.b),
            &self.b,
        ));
        let mut decls = ArenaVec::with_capacity_in(1, &self.b);
        decls.push(VariableDeclarator::new(
            self.span,
            id,
            None,
            Some(init),
            false,
            &self.b,
        ));
        Statement::VariableDeclaration(oxc_allocator::Box::new_in(
            VariableDeclaration::new(
                self.span,
                VariableDeclarationKind::Const,
                decls,
                false,
                &self.b,
            ),
            &self.b,
        ))
    }

    fn namespace_import(&self, local: &str, source: &str) -> Statement<'a> {
        self.import_stmt(
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(oxc_allocator::Box::new_in(
                ImportNamespaceSpecifier::new(self.span, self.binding(local), &self.b),
                &self.b,
            )),
            source,
        )
    }

    fn import_stmt(
        &self,
        specifier: ImportDeclarationSpecifier<'a>,
        source: &str,
    ) -> Statement<'a> {
        let mut specifiers = ArenaVec::with_capacity_in(1, &self.b);
        specifiers.push(specifier);
        Statement::ImportDeclaration(oxc_allocator::Box::new_in(
            ImportDeclaration::new(
                self.span,
                Some(specifiers),
                StringLiteral::new(self.span, self.s(source), None, &self.b),
                None,
                None,
                ImportOrExportKind::Value,
                &self.b,
            ),
            &self.b,
        ))
    }

    fn binding(&self, name: &str) -> BindingIdentifier<'a> {
        BindingIdentifier::new(self.span, self.s(name), &self.b)
    }

    fn inject_import(&self, local: &str, source: &str, named: Option<&str>) -> Statement<'a> {
        let specifier = match named {
            Some(imported) => {
                ImportDeclarationSpecifier::ImportSpecifier(oxc_allocator::Box::new_in(
                    ImportSpecifier::new(
                        self.span,
                        ModuleExportName::IdentifierName(IdentifierName::new(
                            self.span,
                            self.s(imported),
                            &self.b,
                        )),
                        self.binding(local),
                        ImportOrExportKind::Value,
                        &self.b,
                    ),
                    &self.b,
                ))
            }
            None => ImportDeclarationSpecifier::ImportDefaultSpecifier(oxc_allocator::Box::new_in(
                ImportDefaultSpecifier::new(self.span, self.binding(local), &self.b),
                &self.b,
            )),
        };
        self.import_stmt(specifier, source)
    }

    fn var_alias(&self, name: &str, local: &str) -> Statement<'a> {
        let mut decls = ArenaVec::with_capacity_in(1, &self.b);
        decls.push(VariableDeclarator::new(
            self.span,
            BindingPattern::BindingIdentifier(oxc_allocator::Box::new_in(
                self.binding(name),
                &self.b,
            )),
            None,
            Some(self.ident(local)),
            false,
            &self.b,
        ));
        Statement::VariableDeclaration(oxc_allocator::Box::new_in(
            VariableDeclaration::new(
                self.span,
                VariableDeclarationKind::Var,
                decls,
                false,
                &self.b,
            ),
            &self.b,
        ))
    }

    /// Mirrors js_out::inject_call_text (fixed key order, numeric constVal).
    fn inject_call(&self, callee: &str, rule: &StylexRule) -> Statement<'a> {
        let mut props = vec![self.prop(self.prop_key("ltr"), self.string(&rule.ltr))];
        if let Some(rtl) = &rule.rtl {
            props.push(self.prop(self.prop_key("rtl"), self.string(rtl)));
        }
        props.push(self.prop(self.prop_key("priority"), self.number(rule.priority)));
        if let Some(const_key) = &rule.const_key {
            props.push(self.prop(self.prop_key("constKey"), self.string(const_key)));
        }
        if let Some(const_val) = &rule.const_val {
            let value = match const_val_number(const_val) {
                Some(n) => self.number(n),
                None => self.string(&const_val_string(const_val)),
            };
            props.push(self.prop(self.prop_key("constVal"), value));
        }
        let mut args = ArenaVec::with_capacity_in(1, &self.b);
        args.push(Argument::from(self.object(props)));
        let call = Expression::CallExpression(oxc_ast::ast::CallExpression::boxed(
            self.span,
            self.ident(callee),
            None,
            args,
            false,
            &self.b,
        ));
        Statement::ExpressionStatement(oxc_allocator::Box::new_in(
            oxc_ast::ast::ExpressionStatement::new(self.span, call, &self.b),
            &self.b,
        ))
    }

    fn side_effect_import(&self, specifier: &str, source_span: Span) -> Statement<'a> {
        Statement::ImportDeclaration(oxc_allocator::Box::new_in(
            ImportDeclaration::new(
                self.span,
                None,
                StringLiteral::new(source_span, self.s(specifier), None, &self.b),
                None,
                None,
                ImportOrExportKind::Value,
                &self.b,
            ),
            &self.b,
        ))
    }
}

// ---------------------------------------------------------------------------
// Span-addressed clone helpers

/// Clones the outermost expression whose span equals `target` (parity with
/// `expr_text(span)`: the recorded span always names a real expression node).
struct SpanCloner<'x, 'a> {
    alloc: &'a Allocator,
    target: Span,
    found: &'x mut Option<Expression<'a>>,
}

impl<'a> Visit<'a> for SpanCloner<'_, 'a> {
    fn visit_expression(&mut self, it: &Expression<'a>) {
        if self.found.is_some() {
            return;
        }
        if it.span() == self.target {
            *self.found = Some(it.clone_in(self.alloc));
            return;
        }
        walk::walk_expression(self, it);
    }
}

/// [`SpanCloner`] over a span set in one walk: outermost match per span.
struct SpanSetCloner<'x, 'a> {
    alloc: &'a Allocator,
    targets: &'x BTreeSet<SpanKey>,
    found: &'x mut BTreeMap<SpanKey, Expression<'a>>,
}

impl<'a> Visit<'a> for SpanSetCloner<'_, 'a> {
    fn visit_statement(&mut self, it: &Statement<'a>) {
        if span_set_within(self.targets, it.span()) {
            walk::walk_statement(self, it);
        }
    }

    fn visit_expression(&mut self, it: &Expression<'a>) {
        let span = it.span();
        if !span_set_within(self.targets, span) {
            return;
        }
        let k = key(span);
        if self.targets.contains(&k) && !self.found.contains_key(&k) {
            self.found.insert(k, it.clone_in(self.alloc));
        }
        walk::walk_expression(self, it);
    }
}

fn clone_exprs_in_program<'a>(
    alloc: &'a Allocator,
    program: &Program<'a>,
    targets: &BTreeSet<SpanKey>,
) -> BTreeMap<SpanKey, Expression<'a>> {
    let mut found = BTreeMap::new();
    if !targets.is_empty() {
        let mut cloner = SpanSetCloner {
            alloc,
            targets,
            found: &mut found,
        };
        cloner.visit_program(program);
    }
    found
}

/// Node spans nest, so a key starting inside `span` names a node inside it.
fn span_range(span: Span) -> std::ops::RangeInclusive<SpanKey> {
    (span.start, 0)..=(span.end, u32::MAX)
}

fn span_set_within(set: &BTreeSet<SpanKey>, span: Span) -> bool {
    set.range(span_range(span)).next().is_some()
}

fn clone_expr_within<'a>(
    alloc: &'a Allocator,
    root: &Expression<'a>,
    span: Span,
) -> Option<Expression<'a>> {
    let mut found = None;
    let mut cloner = SpanCloner {
        alloc,
        target: span,
        found: &mut found,
    };
    cloner.visit_expression(root);
    found
}

fn clone_expr_within_attr<'a>(
    alloc: &'a Allocator,
    root: &JSXAttributeItem<'a>,
    span: Span,
) -> Option<Expression<'a>> {
    let mut found = None;
    let mut cloner = SpanCloner {
        alloc,
        target: span,
        found: &mut found,
    };
    match root {
        JSXAttributeItem::Attribute(attr) => cloner.visit_jsx_attribute(attr),
        JSXAttributeItem::SpreadAttribute(attr) => cloner.visit_jsx_spread_attribute(attr),
    }
    found
}

// ---------------------------------------------------------------------------
// Plan application

type SpanKey = (u32, u32);

fn key(span: Span) -> SpanKey {
    (span.start, span.end)
}

struct Applier<'a> {
    alloc: &'a Allocator,
    replace: BTreeMap<SpanKey, SynthExpr>,
    jsx: BTreeMap<SpanKey, JsxOp>,
    /// Pre-mutation clones of the dynamic-arrow expression spans (the splice
    /// printer reads them from the raw source, before any nested edit).
    preclones: BTreeMap<SpanKey, Expression<'a>>,
    error: Option<StylexError>,
}

impl<'a> Applier<'a> {
    fn synth(&self, span: Span) -> Synth<'a> {
        Synth {
            b: AstBuilder::new(self.alloc),
            alloc: self.alloc,
            span,
        }
    }

    fn fail(&mut self, error: StylexError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    fn preclone(&self, span: Span) -> Result<Expression<'a>, StylexError> {
        self.preclones
            .get(&key(span))
            .map(|expr| expr.clone_in(self.alloc))
            .ok_or_else(|| {
                internal(format!(
                    "unresolved dynamic span {}..{}",
                    span.start, span.end
                ))
            })
    }

    fn pending_within(&self, span: Span) -> bool {
        self.error.is_none()
            && (self.replace.range(span_range(span)).next().is_some()
                || self.jsx.range(span_range(span)).next().is_some())
    }

    /// `old` is child-mutated before cloning, so table tests carry nested
    /// replacements (the splice path's render-with-inner-edits).
    fn build_expr(
        &self,
        span: Span,
        synth_expr: &SynthExpr,
        old: &Expression<'a>,
    ) -> Result<Expression<'a>, StylexError> {
        let s = self.synth(span);
        match synth_expr {
            SynthExpr::Str(text) => Ok(s.string(text)),
            SynthExpr::ParenValue(value) => Ok(s.paren(s.value(value))),
            SynthExpr::Ident(name) => Ok(s.ident(name)),
            SynthExpr::AtomCallee { hoisted, property } => Ok(s.raw_member(hoisted, property)),
            SynthExpr::CreateObject { map, dynamic } => {
                s.create_object(map, dynamic, &mut |expr_span| self.preclone(expr_span))
            }
            SynthExpr::Table { entries, tests } => {
                let mut test_exprs = Vec::with_capacity(tests.len());
                for test in tests {
                    let cloned = clone_expr_within(self.alloc, old, *test).ok_or_else(|| {
                        internal(format!(
                            "unresolved table test span {}..{}",
                            test.start, test.end
                        ))
                    })?;
                    test_exprs.push(cloned);
                }
                Ok(s.table(entries, test_exprs))
            }
        }
    }

    fn build_jsx(
        &self,
        span: Span,
        op: &JsxOp,
        old: &JSXAttributeItem<'a>,
    ) -> Result<Vec<JSXAttributeItem<'a>>, StylexError> {
        let s = self.synth(span);
        match op {
            JsxOp::Attrs(map) => Ok(s.jsx_attrs(map)),
            JsxOp::SpreadEmptyObject => Ok(vec![s.spread_attr(s.paren(s.object(Vec::new())))]),
            JsxOp::SpreadTable { entries, tests } => {
                let mut test_exprs = Vec::with_capacity(tests.len());
                for test in tests {
                    let cloned =
                        clone_expr_within_attr(self.alloc, old, *test).ok_or_else(|| {
                            internal(format!(
                                "unresolved sx test span {}..{}",
                                test.start, test.end
                            ))
                        })?;
                    test_exprs.push(cloned);
                }
                Ok(vec![s.spread_attr(s.table(entries, test_exprs))])
            }
            JsxOp::SpreadProps { local, expr_span } => {
                let argument =
                    clone_expr_within_attr(self.alloc, old, *expr_span).ok_or_else(|| {
                        internal(format!(
                            "unresolved sx bail span {}..{}",
                            expr_span.start, expr_span.end
                        ))
                    })?;
                Ok(vec![s.spread_attr(s.props_call(local, argument))])
            }
        }
    }

    fn build_stmt(
        &self,
        insert: &InsertOp,
        out: &mut Vec<Statement<'a>>,
    ) -> Result<(), StylexError> {
        let s = self.synth(insert.span);
        match &insert.stmt {
            SynthStmt::ConstDecl { name, value } => {
                let init = match value {
                    HoistValue::StaticChunk(compiled) => s.static_chunk(compiled),
                    HoistValue::CreateObject { map, dynamic } => {
                        s.create_object(map, dynamic, &mut |expr_span| self.preclone(expr_span))?
                    }
                    HoistValue::ParenValue(value) => s.paren(s.value(value)),
                    HoistValue::AtomDynamic {
                        property,
                        prop_key,
                        class_name,
                        var_name,
                    } => s.atom_dynamic(property, prop_key, class_name, var_name),
                };
                out.push(s.const_stmt(name, init));
            }
            SynthStmt::Prologue(stmts) => {
                for stmt in stmts {
                    out.push(match stmt {
                        PrologueStmt::NamespaceImport { local, source } => {
                            s.namespace_import(local, source)
                        }
                        PrologueStmt::InjectImport {
                            local,
                            source,
                            named,
                        } => s.inject_import(local, source, named.as_deref()),
                        PrologueStmt::InjectAlias { name, local } => s.var_alias(name, local),
                    });
                }
            }
            SynthStmt::InjectCall { callee, rule } => out.push(s.inject_call(callee, rule)),
            SynthStmt::SideEffectImport {
                specifier,
                source_span,
            } => out.push(s.side_effect_import(specifier, *source_span)),
        }
        Ok(())
    }
}

impl<'a> VisitMut<'a> for Applier<'a> {
    fn visit_statement(&mut self, stmt: &mut Statement<'a>) {
        if self.pending_within(stmt.span()) {
            walk_mut::walk_statement(self, stmt);
        }
    }

    fn visit_jsx_element(&mut self, el: &mut JSXElement<'a>) {
        if self.pending_within(el.span) {
            walk_mut::walk_jsx_element(self, el);
        }
    }

    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        if !self.pending_within(expr.span()) {
            return;
        }
        // Post-order: nested replacements land before enclosing ones, so a
        // table's cloned tests see their inner replacements (splice parity).
        walk_mut::walk_expression(self, expr);
        if let Some(synth_expr) = self.replace.remove(&key(expr.span())) {
            match self.build_expr(expr.span(), &synth_expr, expr) {
                Ok(built) => *expr = built,
                Err(error) => self.fail(error),
            }
        }
    }

    fn visit_jsx_opening_element(&mut self, el: &mut JSXOpeningElement<'a>) {
        if self.error.is_some() {
            return;
        }
        walk_mut::walk_jsx_opening_element(self, el);
        if !el
            .attributes
            .iter()
            .any(|item| self.jsx.contains_key(&key(item.span())))
        {
            return;
        }
        let old = std::mem::replace(&mut el.attributes, ArenaVec::new_in(&self.alloc));
        for item in old {
            match self.jsx.remove(&key(item.span())) {
                None => el.attributes.push(item),
                Some(op) => match self.build_jsx(item.span(), &op, &item) {
                    Ok(built) => el.attributes.extend(built),
                    Err(error) => {
                        self.fail(error);
                        el.attributes.push(item);
                    }
                },
            }
        }
    }
}

/// Re-anchors comments where the splice text leaves them: before the
/// statements inserted at the offset, else before the next kept statement.
fn retarget_comments(comments: &mut ArenaVec<'_, Comment>, from: u32, to: Option<u32>) {
    for comment in comments.iter_mut() {
        if comment.attached_to == from {
            comment.attached_to = to.unwrap_or(u32::MAX);
        }
    }
}

pub fn apply_plan<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    plan: AstPlan,
) -> Result<(), StylexError> {
    if plan.is_empty() {
        return Ok(());
    }
    let AstPlan {
        replace_exprs,
        jsx_ops,
        inserts: insert_ops,
        removes: remove_ops,
        import_sources,
    } = plan;
    // Collect the pre-mutation clones every dynamic arrow needs.
    let mut wanted: BTreeSet<SpanKey> = BTreeSet::new();
    let mut collect_dynamic = |dynamic: &[DynamicEntry]| {
        for entry in dynamic {
            for var in &entry.compiled.inline_vars {
                wanted.insert(key(var.expr));
            }
            for (_, parts) in &entry.compiled.conditional_props {
                for part in parts {
                    if let ClassPart::Guarded { expr, .. } = part {
                        wanted.insert(key(*expr));
                    }
                }
            }
        }
    };
    for (_, synth_expr) in &replace_exprs {
        if let SynthExpr::CreateObject { dynamic, .. } = synth_expr {
            collect_dynamic(dynamic);
        }
    }
    for insert in &insert_ops {
        if let SynthStmt::ConstDecl {
            value: HoistValue::CreateObject { dynamic, .. },
            ..
        } = &insert.stmt
        {
            collect_dynamic(dynamic);
        }
    }
    let preclones = clone_exprs_in_program(allocator, program, &wanted);

    // parity: the source literal is retargeted in place (Program.exit), so the
    // raw text carries the splice backend's exact quoting.
    for (span, value) in &import_sources {
        for statement in &mut program.body {
            if let Statement::ImportDeclaration(decl) = statement
                && decl.source.span == *span
            {
                decl.source.value = allocator.alloc_str(value).into();
                decl.source.raw = Some(allocator.alloc_str(&js_string_literal(value)).into());
            }
        }
    }

    let mut applier = Applier {
        alloc: allocator,
        replace: replace_exprs
            .into_iter()
            .map(|(span, synth_expr)| (key(span), synth_expr))
            .collect(),
        jsx: jsx_ops
            .into_iter()
            .map(|(span, op)| (key(span), op))
            .collect(),
        preclones,
        error: None,
    };
    applier.visit_program(program);
    if let Some(error) = applier.error.take() {
        return Err(error);
    }
    if !applier.replace.is_empty() || !applier.jsx.is_empty() {
        let leftover: Vec<String> = applier
            .replace
            .keys()
            .chain(applier.jsx.keys())
            .map(|(start, end)| format!("{start}..{end}"))
            .collect();
        return Err(internal(format!(
            "unmatched replacement spans: {}",
            leftover.join(", ")
        )));
    }

    // Statement surgery: removals + anchored insertions over program.body.
    let removes: BTreeMap<SpanKey, &RemoveOp> = remove_ops
        .iter()
        .map(|remove| (key(remove.stmt_span), remove))
        .collect();
    let mut inserts: Vec<&InsertOp> = insert_ops.iter().collect();
    inserts.sort_by_key(|insert| (insert.anchor, insert.seq));
    let mut inserts = inserts.into_iter().peekable();

    let old_body = std::mem::replace(&mut program.body, ArenaVec::new_in(&allocator));
    let mut new_body: Vec<Statement<'a>> = Vec::with_capacity(old_body.len());
    let mut comments = std::mem::replace(&mut program.comments, ArenaVec::new_in(&allocator));
    let mut pending_retarget: Vec<u32> = Vec::new();
    let mut anchors_retargeted: Vec<u32> = Vec::new();
    let handle_insert = |applier: &Applier<'a>,
                         insert: &InsertOp,
                         new_body: &mut Vec<Statement<'a>>,
                         comments: &mut ArenaVec<'a, Comment>,
                         pending_retarget: &mut Vec<u32>,
                         anchors_retargeted: &mut Vec<u32>|
     -> Result<(), StylexError> {
        let at = new_body.len();
        applier.build_stmt(insert, new_body)?;
        if let Some(first) = new_body.get(at) {
            let target = first.span().start;
            // Comments right before the splice offset stay above the inserted
            // block; move their attachment onto its first statement.
            if target != insert.anchor && !anchors_retargeted.contains(&insert.anchor) {
                retarget_comments(comments, insert.anchor, Some(target));
                anchors_retargeted.push(insert.anchor);
            }
            for from in pending_retarget.drain(..) {
                retarget_comments(comments, from, Some(target));
            }
        }
        Ok(())
    };
    for stmt in old_body {
        let stmt_start = stmt.span().start;
        while let Some(insert) = inserts.peek() {
            if insert.anchor > stmt_start {
                break;
            }
            let insert = inserts.next().expect("peeked");
            handle_insert(
                &applier,
                insert,
                &mut new_body,
                &mut comments,
                &mut pending_retarget,
                &mut anchors_retargeted,
            )?;
        }
        match removes.get(&key(stmt.span())) {
            None => {
                for from in pending_retarget.drain(..) {
                    retarget_comments(&mut comments, from, Some(stmt_start));
                }
                new_body.push(stmt);
            }
            Some(remove) if remove.indices.len() >= remove.decl_count => {
                pending_retarget.push(stmt_start);
            }
            Some(remove) => {
                let mut stmt = stmt;
                retain_declarators(allocator, &mut stmt, &remove.indices);
                new_body.push(stmt);
            }
        }
    }
    for insert in inserts {
        handle_insert(
            &applier,
            insert,
            &mut new_body,
            &mut comments,
            &mut pending_retarget,
            &mut anchors_retargeted,
        )?;
    }
    for from in pending_retarget.drain(..) {
        retarget_comments(&mut comments, from, None);
    }
    program.body.extend(new_body);
    program.comments = comments;
    Ok(())
}

fn retain_declarators<'a>(allocator: &'a Allocator, stmt: &mut Statement<'a>, removed: &[usize]) {
    let decl = match stmt {
        Statement::VariableDeclaration(decl) => &mut **decl,
        Statement::ExportDeclaration(export) => match &mut export.declaration {
            Declaration::VariableDeclaration(decl) => &mut **decl,
            _ => return,
        },
        _ => return,
    };
    let old = std::mem::replace(&mut decl.declarations, ArenaVec::new_in(&allocator));
    for (index, declarator) in old.into_iter().enumerate() {
        if !removed.contains(&index) {
            decl.declarations.push(declarator);
        }
    }
}

/// Debug-facing invariant check: every span in the program is in-range and
/// well-formed (start <= end <= source length).
pub fn assert_spans_in_range(program: &Program<'_>, source_len: u32) -> Result<(), String> {
    struct SpanCheck {
        len: u32,
        bad: Vec<String>,
    }
    impl<'a> Visit<'a> for SpanCheck {
        fn visit_expression(&mut self, it: &Expression<'a>) {
            self.check(it.span());
            walk::walk_expression(self, it);
        }
        fn visit_statement(&mut self, it: &oxc_ast::ast::Statement<'a>) {
            self.check(it.span());
            walk::walk_statement(self, it);
        }
    }
    impl SpanCheck {
        fn check(&mut self, span: Span) {
            if span.start > span.end || span.end > self.len {
                self.bad
                    .push(format!("{}..{} (len {})", span.start, span.end, self.len));
            }
        }
    }
    let mut check = SpanCheck {
        len: source_len,
        bad: Vec::new(),
    };
    check.visit_program(program);
    if check.bad.is_empty() {
        Ok(())
    } else {
        Err(check.bad.join(", "))
    }
}
