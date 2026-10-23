//! Compile-time partial evaluator over the oxc AST, default-deny past the
//! verified surface. parity: evaluate-path.js + parse-stylex-create-arg.js

pub mod cross_file;
pub mod functions;
pub mod methods;
pub mod value;

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::BTreeMap;

use crate::fxhash::FxHashMap;
use std::rc::Rc;
use std::sync::Arc;

use oxc_ast::ast::{
    Argument, ArrayExpressionElement, BinaryExpression, CallExpression, ChainElement, Expression,
    IdentifierReference, LogicalExpression, ObjectExpression, ObjectPropertyKind, PropertyKey,
    PropertyKind, TemplateLiteral, UnaryExpression,
};
use oxc_span::{GetSpan, Span};
use oxc_syntax::operator::{BinaryOperator, LogicalOperator, UnaryOperator};
use oxc_syntax::symbol::SymbolId;

use crate::errors::{ErrorCode, StylexError};
use crate::eval::cross_file as errs;
use crate::eval::cross_file::VarGroupProxy;
use crate::eval::functions::{EvalCallables, StylexCallable, WHEN_MEMBERS};
use crate::eval::methods::ArrowCaller;
use crate::eval::value::{EvalValue, JsObjectMap, array_index};
use crate::imports::{ImportTable, ImportedSymbol, StylexNamedImport};
use crate::jsrt::{js_number_to_string, js_slice_utf16, js_slice_utf16_checked, utf16_cmp};
use crate::module_resolution::FsProvider;
use crate::shared::dynamic::{DynamicFn, inline_style_for_leaf};
use crate::state::{BindingDecl, CompileState};

/// Evaluator-local value model; `Rc` containers keep JS allocation identity
/// through clones (`x === x` via the node cache), like real objects upstream.
#[derive(Debug, Clone)]
pub enum JsValue {
    Null,
    Undefined,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Rc<Vec<JsValue>>),
    Obj(Rc<JsObj>),
    Proxy(Rc<VarGroupProxy>),
    Callable(Callable),
}

impl JsValue {
    pub fn array(items: Vec<JsValue>) -> Self {
        JsValue::Arr(Rc::new(items))
    }

    pub fn object(obj: JsObj) -> Self {
        JsValue::Obj(Rc::new(obj))
    }

    pub fn proxy(proxy: VarGroupProxy) -> Self {
        JsValue::Proxy(Rc::new(proxy))
    }
}

#[derive(Debug, Clone)]
pub enum Callable {
    Stylex(StylexCallable),
    /// A local expression-body arrow, callable at compile time; the key is
    /// its span start in the evaluator's arrow-body table.
    Arrow(u32),
    /// Function values calls never dispatch to.
    Opaque,
}

/// Insertion-ordered object; ES OwnPropertyKeys ordering is restored when the
/// value converts to `JsObjectMap`, which every observable path goes through.
#[derive(Debug, Clone)]
pub struct JsObj {
    repr: ObjRepr,
}

#[derive(Debug, Clone)]
enum ObjRepr {
    Built {
        entries: Vec<(String, JsValue)>,
        /// Lazy key→position table so wide objects avoid quadratic scans.
        // Boxed to keep the usually-None field one word in every JsObj.
        #[allow(clippy::box_collection)]
        index: Option<Box<FxHashMap<String, usize>>>,
        /// CSSType `instanceof` brand (the syntax): out-of-band like a prototype —
        /// spreads copy entries only, aliases share the allocation.
        css_type: Option<String>,
    },
    /// A compiled map read in place; a slot converts once so repeat reads of
    /// one entry keep one identity (`===`), like a real property.
    Frozen {
        map: Arc<JsObjectMap>,
        slots: OnceCell<Box<[OnceCell<JsValue>]>>,
    },
}

type FrozenSlots = OnceCell<Box<[OnceCell<JsValue>]>>;

fn frozen_slot<'o>(map: &'o JsObjectMap, slots: &'o FrozenSlots, i: usize) -> &'o JsValue {
    let slots = slots.get_or_init(|| {
        std::iter::repeat_with(OnceCell::new)
            .take(map.len())
            .collect()
    });
    slots[i].get_or_init(|| from_eval_value(map.entry_at(i).1))
}

enum Entries<'o> {
    Built(std::slice::Iter<'o, (String, JsValue)>),
    Frozen {
        map: &'o JsObjectMap,
        slots: &'o FrozenSlots,
        next: usize,
    },
}

impl<'o> Iterator for Entries<'o> {
    type Item = (&'o str, &'o JsValue);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Entries::Built(iter) => iter.next().map(|(k, v)| (k.as_str(), v)),
            Entries::Frozen { map, slots, next } => {
                let i = *next;
                if i >= map.len() {
                    return None;
                }
                *next += 1;
                Some((map.entry_at(i).0, frozen_slot(map, slots, i)))
            }
        }
    }
}

impl Default for JsObj {
    fn default() -> Self {
        Self::built(Vec::new())
    }
}

impl JsObj {
    fn built(entries: Vec<(String, JsValue)>) -> Self {
        Self {
            repr: ObjRepr::Built {
                entries,
                index: None,
                css_type: None,
            },
        }
    }

    pub fn with_capacity(n: usize) -> Self {
        Self::built(Vec::with_capacity(n))
    }

    /// The object sequential `insert` would build from `entries`, which must
    /// not repeat a key (callers hand over a map's entries).
    pub fn from_unique_entries(entries: Vec<(String, JsValue)>) -> Self {
        debug_assert!(
            entries
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<crate::fxhash::FxHashSet<_>>()
                .len()
                == entries.len()
        );
        Self::built(entries)
    }

    pub fn frozen(map: Arc<JsObjectMap>) -> Self {
        Self {
            repr: ObjRepr::Frozen {
                map,
                slots: OnceCell::new(),
            },
        }
    }

    fn frozen_map(&self) -> Option<&Arc<JsObjectMap>> {
        match &self.repr {
            ObjRepr::Frozen { map, .. } => Some(map),
            ObjRepr::Built { .. } => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&JsValue> {
        match &self.repr {
            ObjRepr::Built { entries, index, .. } => match index {
                Some(index) => index.get(key).map(|&i| &entries[i].1),
                None => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            },
            ObjRepr::Frozen { map, slots } => {
                let i = map.position(key)?;
                Some(frozen_slot(map, slots, i))
            }
        }
    }

    pub fn insert(&mut self, key: String, value: JsValue) {
        self.materialize();
        let ObjRepr::Built { entries, index, .. } = &mut self.repr else {
            unreachable!("materialize leaves a built object");
        };
        if index.is_none() && entries.len() >= value::NAMED_INDEX_THRESHOLD {
            *index = Some(Box::new(
                entries
                    .iter()
                    .enumerate()
                    .map(|(i, (k, _))| (k.clone(), i))
                    .collect(),
            ));
        }
        if let Some(index) = index {
            match index.entry(key) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    entries[*e.get()].1 = value;
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    entries.push((e.key().clone(), value));
                    e.insert(entries.len() - 1);
                }
            }
        } else if let Some(entry) = entries.iter_mut().find(|(k, _)| *k == key) {
            entry.1 = value;
        } else {
            entries.push((key, value));
        }
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &JsValue)> {
        match &self.repr {
            ObjRepr::Built { entries, .. } => Entries::Built(entries.iter()),
            ObjRepr::Frozen { map, slots } => Entries::Frozen {
                map,
                slots,
                next: 0,
            },
        }
    }

    pub fn css_type(&self) -> Option<&str> {
        match &self.repr {
            ObjRepr::Built { css_type, .. } => css_type.as_deref(),
            ObjRepr::Frozen { map, .. } => map.css_type(),
        }
    }

    pub fn set_css_type(&mut self, syntax: String) {
        self.materialize();
        if let ObjRepr::Built { css_type, .. } = &mut self.repr {
            *css_type = Some(syntax);
        }
    }

    /// Frozen → Built; slots already handed out keep their identity.
    fn materialize(&mut self) {
        let ObjRepr::Frozen { map, slots } = &mut self.repr else {
            return;
        };
        let mut slots = slots.take();
        let entries = map
            .entries()
            .enumerate()
            .map(|(i, (k, v))| {
                let value = slots
                    .as_mut()
                    .and_then(|s| s[i].take())
                    .unwrap_or_else(|| from_eval_value(v));
                (k.to_string(), value)
            })
            .collect();
        let css_type = map.css_type().map(str::to_string);
        self.repr = ObjRepr::Built {
            entries,
            index: None,
            css_type,
        };
    }

    fn into_map(self) -> Arc<JsObjectMap> {
        match self.repr {
            ObjRepr::Built {
                entries, css_type, ..
            } => {
                let entries = entries
                    .into_iter()
                    .map(|(k, v)| (k, into_eval_value(v)))
                    .collect();
                let mut map = JsObjectMap::from_unique_entries(entries);
                if let Some(syntax) = css_type {
                    map.set_css_type(syntax);
                }
                Arc::new(map)
            }
            ObjRepr::Frozen { map, .. } => map,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Deopt {
    pub reason: Cow<'static, str>,
    pub span: Span,
}

#[derive(Debug)]
pub enum EvalOutcome {
    Value(JsValue),
    /// Upstream deopt: hard error in create/defineVars contexts, bail-to-runtime
    /// marker in props()/stylex() merge contexts.
    NonStatic(Deopt),
}

pub type EvalResult = Result<EvalOutcome, StylexError>;

/// Proxies convert to empty objects (the upstream JS Proxy has no own keys);
/// functions to `undefined` (downstream validation rejects both identically).
pub fn to_eval_value(value: &JsValue) -> EvalValue {
    match value {
        JsValue::Null => EvalValue::Null,
        JsValue::Undefined | JsValue::Callable(_) => EvalValue::Undefined,
        JsValue::Bool(b) => EvalValue::Bool(*b),
        JsValue::Num(n) => EvalValue::Num(*n),
        JsValue::Str(s) => EvalValue::Str(s.clone()),
        JsValue::Arr(items) => EvalValue::Arr(items.iter().map(to_eval_value).collect()),
        JsValue::Obj(obj) => EvalValue::Obj(obj_to_map(obj)),
        JsValue::Proxy(_) => EvalValue::Obj(JsObjectMap::new().into()),
    }
}

fn obj_to_map(obj: &JsObj) -> Arc<JsObjectMap> {
    if let Some(map) = obj.frozen_map() {
        return Arc::clone(map);
    }
    let entries = obj
        .entries()
        .map(|(k, v)| (k.to_string(), to_eval_value(v)))
        .collect();
    let mut map = JsObjectMap::from_unique_entries(entries);
    if let Some(syntax) = obj.css_type() {
        map.set_css_type(syntax.to_string());
    }
    Arc::new(map)
}

/// `to_eval_value` for a value the caller is finished with: strings and
/// uniquely owned containers move; aliased containers fall back to copying.
pub fn into_eval_value(value: JsValue) -> EvalValue {
    match value {
        JsValue::Null => EvalValue::Null,
        JsValue::Undefined | JsValue::Callable(_) => EvalValue::Undefined,
        JsValue::Bool(b) => EvalValue::Bool(b),
        JsValue::Num(n) => EvalValue::Num(n),
        JsValue::Str(s) => EvalValue::Str(s),
        JsValue::Arr(items) => match Rc::try_unwrap(items) {
            Ok(items) => EvalValue::Arr(items.into_iter().map(into_eval_value).collect()),
            Err(shared) => EvalValue::Arr(shared.iter().map(to_eval_value).collect()),
        },
        JsValue::Obj(obj) => EvalValue::Obj(match Rc::try_unwrap(obj) {
            Ok(obj) => obj.into_map(),
            Err(shared) => obj_to_map(&shared),
        }),
        JsValue::Proxy(_) => EvalValue::Obj(JsObjectMap::new().into()),
    }
}

pub fn from_eval_value(value: &EvalValue) -> JsValue {
    match value {
        EvalValue::Null => JsValue::Null,
        EvalValue::Undefined => JsValue::Undefined,
        EvalValue::Bool(b) => JsValue::Bool(*b),
        EvalValue::Num(n) => JsValue::Num(*n),
        EvalValue::Str(s) => JsValue::Str(s.clone()),
        EvalValue::Arr(items) => JsValue::array(items.iter().map(from_eval_value).collect()),
        EvalValue::Obj(map) => JsValue::object(JsObj::frozen(Arc::clone(map))),
    }
}

/// `from_eval_value` for a value the caller is finished with.
pub fn into_js_value(value: EvalValue) -> JsValue {
    match value {
        EvalValue::Null => JsValue::Null,
        EvalValue::Undefined => JsValue::Undefined,
        EvalValue::Bool(b) => JsValue::Bool(b),
        EvalValue::Num(n) => JsValue::Num(n),
        EvalValue::Str(s) => JsValue::Str(s),
        EvalValue::Arr(items) => JsValue::array(items.into_iter().map(into_js_value).collect()),
        EvalValue::Obj(map) => JsValue::object(JsObj::frozen(map)),
    }
}

#[derive(Debug, Clone)]
enum RegistryEntry {
    Callable(StylexCallable),
    Value(JsValue),
}

/// Outcome of the babel-`resolve()` walk over declarator inits.
enum Resolved<'a> {
    Override(EvalValue),
    Expr(&'a Expression<'a>),
    Identifier(&'a IdentifierReference<'a>),
    /// Declarator without an initializer (babel: null init path).
    NullInit,
    /// A non-init declaration node, named as babel would report it.
    DeclNode(&'static str, Span),
}

impl Resolved<'_> {
    fn span(&self) -> Option<Span> {
        match self {
            Resolved::Expr(expr) => Some(expr.span()),
            Resolved::Identifier(id) => Some(id.span),
            Resolved::DeclNode(_, span) => Some(*span),
            Resolved::Override(_) | Resolved::NullInit => None,
        }
    }
}

/// The `FunctionConfig` equivalent the visitor hands the evaluator.
// parity: stylex-create.js:171-206 + state-manager.js applyStylexEnv
#[derive(Debug, Clone, Default)]
pub struct FunctionRegistry {
    identifiers: BTreeMap<String, RegistryEntry>,
    member_callables: Vec<(String, &'static str, StylexCallable)>,
}

fn when_object() -> JsValue {
    let mut obj = JsObj::default();
    for (name, callable) in WHEN_MEMBERS {
        obj.insert(
            name.to_string(),
            JsValue::Callable(Callable::Stylex(callable)),
        );
    }
    JsValue::object(obj)
}

impl FunctionRegistry {
    /// FunctionConfig of visitors/stylex-keyframes.js: firstThatWorks + env only.
    pub fn for_keyframes(imports: &ImportTable, env: &EvalValue) -> Self {
        let mut registry = Self::restricted_base(imports, env);
        for (local, binding) in &imports.named {
            if *binding == StylexNamedImport::FirstThatWorks {
                registry.identifiers.insert(
                    local.clone(),
                    RegistryEntry::Callable(StylexCallable::FirstThatWorks),
                );
            }
        }
        for namespace in &imports.stylex_namespaces {
            registry.add_member_callable(
                namespace,
                "firstThatWorks",
                StylexCallable::FirstThatWorks,
            );
        }
        registry
    }

    /// FunctionConfig of visitors/stylex-props.js: defaultMarker + env only.
    pub fn for_props(imports: &ImportTable, env: &EvalValue) -> Self {
        let mut registry = Self::restricted_base(imports, env);
        for (local, binding) in &imports.named {
            if *binding == StylexNamedImport::DefaultMarker {
                registry.identifiers.insert(
                    local.clone(),
                    RegistryEntry::Callable(StylexCallable::DefaultMarker),
                );
            }
        }
        for namespace in &imports.stylex_namespaces {
            registry.add_member_callable(namespace, "defaultMarker", StylexCallable::DefaultMarker);
        }
        registry
    }

    /// FunctionConfig of visitors/stylex-define-consts.js: env only.
    pub fn for_consts(imports: &ImportTable, env: &EvalValue) -> Self {
        Self::restricted_base(imports, env)
    }

    /// FunctionConfig of visitors/stylex-view-transition-class.js:
    /// firstThatWorks + keyframes + env.
    pub fn for_view_transition(imports: &ImportTable, env: &EvalValue) -> Self {
        let mut registry = Self::for_keyframes(imports, env);
        for (local, binding) in &imports.named {
            if *binding == StylexNamedImport::Keyframes {
                registry.identifiers.insert(
                    local.clone(),
                    RegistryEntry::Callable(StylexCallable::Keyframes),
                );
            }
        }
        for namespace in &imports.stylex_namespaces {
            registry.add_member_callable(namespace, "keyframes", StylexCallable::Keyframes);
        }
        registry
    }

    pub(crate) fn add_member_callable(
        &mut self,
        object: &str,
        prop: &'static str,
        callable: StylexCallable,
    ) {
        match self
            .member_callables
            .iter_mut()
            .find(|(o, p, _)| o == object && *p == prop)
        {
            Some(slot) => slot.2 = callable,
            None => self
                .member_callables
                .push((object.to_string(), prop, callable)),
        }
    }

    pub(crate) fn member_callable(&self, object: &str, prop: &str) -> Option<&StylexCallable> {
        self.member_callables
            .iter()
            .find(|(o, p, _)| o == object && *p == prop)
            .map(|(_, _, callable)| callable)
    }

    fn restricted_base(imports: &ImportTable, env: &EvalValue) -> Self {
        let mut registry = FunctionRegistry::default();
        for (local, binding) in &imports.named {
            if *binding == StylexNamedImport::Env {
                registry
                    .identifiers
                    .insert(local.clone(), RegistryEntry::Value(from_eval_value(env)));
            }
        }
        for namespace in &imports.stylex_namespaces {
            let mut obj = JsObj::default();
            obj.insert("env".to_string(), from_eval_value(env));
            registry.identifiers.insert(
                namespace.clone(),
                RegistryEntry::Value(JsValue::object(obj)),
            );
        }
        registry
    }

    pub fn for_create(imports: &ImportTable, env: &EvalValue) -> Self {
        let mut registry = FunctionRegistry::default();
        for (local, binding) in &imports.named {
            let entry = match binding {
                StylexNamedImport::FirstThatWorks => {
                    RegistryEntry::Callable(StylexCallable::FirstThatWorks)
                }
                StylexNamedImport::Keyframes => RegistryEntry::Callable(StylexCallable::Keyframes),
                StylexNamedImport::PositionTry => {
                    RegistryEntry::Callable(StylexCallable::PositionTry)
                }
                StylexNamedImport::DefaultMarker => {
                    RegistryEntry::Callable(StylexCallable::DefaultMarker)
                }
                StylexNamedImport::When => RegistryEntry::Value(when_object()),
                StylexNamedImport::Env => RegistryEntry::Value(from_eval_value(env)),
                _ => continue,
            };
            registry.identifiers.insert(local.clone(), entry);
        }
        for namespace in &imports.stylex_namespaces {
            let mut obj = JsObj::default();
            obj.insert("when".to_string(), when_object());
            obj.insert("env".to_string(), from_eval_value(env));
            registry.identifiers.insert(
                namespace.clone(),
                RegistryEntry::Value(JsValue::object(obj)),
            );
            for (name, callable) in [
                ("firstThatWorks", StylexCallable::FirstThatWorks),
                ("keyframes", StylexCallable::Keyframes),
                ("positionTry", StylexCallable::PositionTry),
                ("defaultMarker", StylexCallable::DefaultMarker),
            ] {
                registry.add_member_callable(namespace, name, callable);
            }
        }
        registry
    }
}

pub struct Evaluator<'a, 'env> {
    pub state: &'env mut CompileState<'a>,
    pub fs: &'env dyn FsProvider,
    pub callables: &'env dyn EvalCallables,
    /// Arrow parameter binding writes through `to_mut`, so a borrowed registry
    /// is copied only by evaluations that call arrows.
    registry: Cow<'env, FunctionRegistry>,
    /// parity: FunctionConfig.disableImports — named-import resolution off.
    disable_imports: bool,
    arrow_bodies: BTreeMap<u32, (&'a Expression<'a>, Vec<String>)>,
    /// Off by default: the transform path still hard-errors on dynamic styles.
    dynamic_namespaces: bool,
    depth: u32,
    /// parity: evaluateCached `seen` — per-`evaluate()` node cache, giving
    /// same-binding references the same (Rc-identical) value.
    seen: FxHashMap<(u32, u32), SeenEntry>,
}

#[derive(Debug, Clone)]
enum SeenEntry {
    InFlight,
    Value(JsValue),
}

macro_rules! value_or_return {
    ($self:expr, $expr:expr) => {
        match $self.eval_node($expr)? {
            EvalOutcome::Value(v) => v,
            deopt => return Ok(deopt),
        }
    };
}

impl<'a, 'env> Evaluator<'a, 'env> {
    pub fn for_create(
        state: &'env mut CompileState<'a>,
        fs: &'env dyn FsProvider,
        callables: &'env dyn EvalCallables,
    ) -> Self {
        let registry = FunctionRegistry::for_create(&state.imports, &state.options.env);
        Evaluator {
            state,
            fs,
            callables,
            registry: Cow::Owned(registry),
            disable_imports: false,
            arrow_bodies: BTreeMap::new(),
            dynamic_namespaces: false,
            depth: 0,
            seen: FxHashMap::default(),
        }
    }

    /// Enables the parse-stylex-create-arg.js arrow-namespace path.
    pub fn allow_dynamic_namespaces(&mut self) {
        self.dynamic_namespaces = true;
    }

    pub fn with_registry(
        state: &'env mut CompileState<'a>,
        fs: &'env dyn FsProvider,
        callables: &'env dyn EvalCallables,
        registry: FunctionRegistry,
        disable_imports: bool,
    ) -> Self {
        Self::with_registry_cow(state, fs, callables, Cow::Owned(registry), disable_imports)
    }

    pub fn with_registry_ref(
        state: &'env mut CompileState<'a>,
        fs: &'env dyn FsProvider,
        callables: &'env dyn EvalCallables,
        registry: &'env FunctionRegistry,
        disable_imports: bool,
    ) -> Self {
        Self::with_registry_cow(
            state,
            fs,
            callables,
            Cow::Borrowed(registry),
            disable_imports,
        )
    }

    fn with_registry_cow(
        state: &'env mut CompileState<'a>,
        fs: &'env dyn FsProvider,
        callables: &'env dyn EvalCallables,
        registry: Cow<'env, FunctionRegistry>,
        disable_imports: bool,
    ) -> Self {
        Evaluator {
            state,
            fs,
            callables,
            registry,
            disable_imports,
            arrow_bodies: BTreeMap::new(),
            dynamic_namespaces: false,
            depth: 0,
            seen: FxHashMap::default(),
        }
    }

    fn deopt(&self, span: Span, reason: impl Into<Cow<'static, str>>) -> EvalResult {
        Ok(EvalOutcome::NonStatic(Deopt {
            reason: reason.into(),
            span,
        }))
    }

    fn value(&self, value: JsValue) -> EvalResult {
        Ok(EvalOutcome::Value(value))
    }

    /// A fresh top-level `evaluate()` call (fresh `seen`, as upstream defaults).
    pub fn eval_entry(&mut self, expr: &'a Expression<'a>) -> EvalResult {
        let _t = crate::timings::start(crate::timings::Stage::Eval);
        self.seen.clear();
        self.eval(expr)
    }

    /// Converts a finished `eval_entry` result; the node cache still holds Rc
    /// clones of its tree and would otherwise force `into_eval_value` to copy.
    fn consume(&mut self, value: JsValue) -> EvalValue {
        self.seen.clear();
        into_eval_value(value)
    }

    /// parity: nested `evaluate(...)` without the shared seen map (array
    /// elements, call-object fallback, arrow-closure bodies).
    fn eval_fresh(&mut self, expr: &'a Expression<'a>) -> EvalResult {
        let saved = std::mem::take(&mut self.seen);
        let result = self.eval(expr);
        self.seen = saved;
        result
    }

    // parity: evaluateCached — value hits replay (preserving identity), and a
    // node already in flight deopts with literally "Currently evaluating".
    pub fn eval(&mut self, expr: &'a Expression<'a>) -> EvalResult {
        let span = expr.span();
        self.eval_cached(span, |ev| ev.eval_inner(expr))
    }

    // Only binding resolution (`eval`/`eval_cached`) can revisit a node within
    // one `seen` lifetime; structural children skip the cache.
    fn eval_node(&mut self, expr: &'a Expression<'a>) -> EvalResult {
        self.enter()?;
        let result = self.eval_inner(expr);
        self.depth -= 1;
        result
    }

    fn enter(&mut self) -> Result<(), StylexError> {
        self.depth += 1;
        if self.depth > 128 {
            self.depth -= 1;
            return Err(StylexError::new(
                ErrorCode::NonStaticValue,
                "StyleX evaluation exceeded the recursion limit.",
            ));
        }
        Ok(())
    }

    fn eval_cached(
        &mut self,
        span: Span,
        eval_fn: impl FnOnce(&mut Self) -> EvalResult,
    ) -> EvalResult {
        self.enter()?;
        let key = (span.start, span.end);
        match self.seen.get(&key) {
            Some(SeenEntry::InFlight) => {
                self.depth -= 1;
                return self.deopt(span, "Currently evaluating");
            }
            Some(SeenEntry::Value(v)) => {
                let v = v.clone();
                self.depth -= 1;
                return self.value(v);
            }
            None => {}
        }
        self.seen.insert(key, SeenEntry::InFlight);
        let result = eval_fn(self);
        if let Ok(EvalOutcome::Value(v)) = &result {
            self.seen.insert(key, SeenEntry::Value(v.clone()));
        }
        self.depth -= 1;
        result
    }

    fn eval_inner(&mut self, expr: &'a Expression<'a>) -> EvalResult {
        match expr {
            Expression::ParenthesizedExpression(e) => self.eval_node(&e.expression),
            Expression::TSAsExpression(e) => self.eval_node(&e.expression),
            Expression::TSSatisfiesExpression(e) => self.eval_node(&e.expression),
            Expression::ArrowFunctionExpression(arrow) => {
                let simple_params = arrow.params.rest.is_none();
                let expression_body =
                    !matches!(arrow.body, oxc_ast::ast::ArrowFunctionBody::FunctionBody(_));
                // parity: evaluate-path.js:369 builds a callable closure only
                // for direct-identifier params; anything else deopts.
                let strict: Option<Vec<String>> = arrow
                    .params
                    .items
                    .iter()
                    .map(|p| match &p.pattern {
                        // Defaulted params are babel AssignmentPatterns: no closure.
                        oxc_ast::ast::BindingPattern::BindingIdentifier(id)
                            if p.initializer.is_none() =>
                        {
                            Some(id.name.to_string())
                        }
                        _ => None,
                    })
                    .collect();
                if let (true, Some(params), Some(body)) = (
                    simple_params && expression_body,
                    strict,
                    arrow.body.as_expression(),
                ) {
                    self.arrow_bodies.insert(arrow.span.start, (body, params));
                    self.value(JsValue::Callable(Callable::Arrow(arrow.span.start)))
                } else {
                    self.deopt(
                        arrow.span,
                        errs::unsupported_expression("ArrowFunctionExpression"),
                    )
                }
            }
            Expression::SequenceExpression(seq) => match seq.expressions.last() {
                Some(last) => self.eval_node(last),
                None => self.deopt(seq.span, errs::PATH_WITHOUT_NODE),
            },
            Expression::StringLiteral(lit) => {
                if lit.lone_surrogates {
                    return Err(StylexError::lone_surrogate("a string literal"));
                }
                self.value(JsValue::Str(lit.value.to_string()))
            }
            Expression::NumericLiteral(lit) => self.value(JsValue::Num(lit.value)),
            Expression::BooleanLiteral(lit) => self.value(JsValue::Bool(lit.value)),
            Expression::NullLiteral(_) => self.value(JsValue::Null),
            Expression::TemplateLiteral(tpl) => self.eval_template(tpl),
            Expression::ConditionalExpression(cond) => {
                let test = value_or_return!(self, &cond.test);
                if truthy(&test) {
                    self.eval_node(&cond.consequent)
                } else {
                    self.eval_node(&cond.alternate)
                }
            }
            Expression::StaticMemberExpression(_) | Expression::ComputedMemberExpression(_) => {
                self.eval_member(expr)
            }
            Expression::Identifier(id) => self.eval_identifier(id),
            Expression::UnaryExpression(unary) => self.eval_unary(unary),
            Expression::ArrayExpression(array) => {
                let mut out = Vec::with_capacity(array.elements.len());
                for element in &array.elements {
                    match element {
                        ArrayExpressionElement::Elision(elision) => {
                            return self.deopt(elision.span, errs::PATH_WITHOUT_NODE);
                        }
                        ArrayExpressionElement::SpreadElement(spread) => {
                            return self
                                .deopt(spread.span, errs::unsupported_expression("SpreadElement"));
                        }
                        _ => {
                            let expr = element.to_expression();
                            match self.eval_fresh(expr)? {
                                EvalOutcome::Value(v) => out.push(v),
                                deopt => return Ok(deopt),
                            }
                        }
                    }
                }
                self.value(JsValue::array(out))
            }
            Expression::ObjectExpression(object) => self.eval_object(object),
            Expression::LogicalExpression(logical) => self.eval_logical(logical),
            Expression::BinaryExpression(binary) => self.eval_binary(binary),
            Expression::CallExpression(call) => self.eval_call(call),
            Expression::ChainExpression(chain) => {
                let name = match &chain.expression {
                    ChainElement::CallExpression(_) => "OptionalCallExpression",
                    ChainElement::TSNonNullExpression(_) => "TSNonNullExpression",
                    _ => "OptionalMemberExpression",
                };
                self.deopt(chain.span, errs::unsupported_expression(name))
            }
            other => self.deopt(
                other.span(),
                errs::unsupported_expression(babel_type_name(other)),
            ),
        }
    }

    fn eval_template(&mut self, tpl: &'a TemplateLiteral<'a>) -> EvalResult {
        let mut out = String::with_capacity(tpl.quasis.iter().map(|q| q.value.raw.len()).sum());
        for (i, quasi) in tpl.quasis.iter().enumerate() {
            if quasi.lone_surrogates {
                return Err(StylexError::lone_surrogate("a template literal"));
            }
            match &quasi.value.cooked {
                Some(cooked) => out.push_str(cooked),
                None => out.push_str("undefined"),
            }
            if let Some(expr) = tpl.expressions.get(i) {
                let value = value_or_return!(self, expr);
                match js_to_string(&value) {
                    Some(s) => out.push_str(&s),
                    None => {
                        return self.deopt(
                            expr.span(),
                            errs::unsupported_expression(babel_type_name(expr)),
                        );
                    }
                }
            }
        }
        self.value(JsValue::Str(out))
    }

    // parity: _evaluate isReferencedIdentifier branch — real scope resolution
    // through oxc_semantic, then babel resolve() through declarator inits.
    fn eval_identifier(&mut self, id: &'a IdentifierReference<'a>) -> EvalResult {
        let name = id.name.as_str();
        let span = id.span;
        if let Some(entry) = self.registry.identifiers.get(name) {
            return self.value(match entry {
                RegistryEntry::Callable(c) => JsValue::Callable(Callable::Stylex(c.clone())),
                RegistryEntry::Value(v) => v.clone(),
            });
        }
        let Some(symbol) = self.state.symbol_of(id) else {
            return match name {
                "undefined" => self.value(JsValue::Undefined),
                "Infinity" => self.value(JsValue::Num(f64::INFINITY)),
                "NaN" => self.value(JsValue::Num(f64::NAN)),
                _ => self.deopt(span, errs::UNDEFINED_CONST),
            };
        };
        let info = self.state.binding_info(symbol);
        if matches!(info.decl, BindingDecl::NamedImport) && !self.disable_imports {
            return self.eval_named_import(symbol, name, span);
        }
        if matches!(info.decl, BindingDecl::DefaultImport) {
            return self.deopt(span, errs::IMPORT_FILE_EVAL_ERROR);
        }
        if self.state.is_non_constant(symbol) || self.state.is_mutated(symbol) {
            return self.deopt(span, errs::NON_CONSTANT);
        }
        if span.start < info.span.end {
            return self.deopt(span, errs::USED_BEFORE_DECLARATION);
        }
        if matches!(name, "undefined" | "Infinity" | "NaN") {
            return self.deopt(span, errs::UNINITIALIZED_CONST);
        }
        // babel resolve(): `binding.kind === 'module'` fails resolution.
        if matches!(
            info.decl,
            BindingDecl::NamedImport | BindingDecl::DefaultImport | BindingDecl::NamespaceImport
        ) {
            return self.deopt(span, errs::UNDEFINED_CONST);
        }
        let mut visited: Vec<(u32, u32)> = vec![(span.start, span.end)];
        let resolved = self.resolve_symbol_decl(symbol, &mut visited);
        if let Some(rspan) = resolved.span()
            && (rspan == span || (rspan.start <= span.start && span.end <= rspan.end))
        {
            // babel: `resolved === path` (or resolved is an ancestor).
            return self.deopt(span, errs::UNDEFINED_CONST);
        }
        match resolved {
            Resolved::Override(value) => self.value(into_js_value(value)),
            Resolved::Expr(expr) => self.eval(expr),
            Resolved::Identifier(other) => {
                self.eval_cached(other.span, |ev| ev.eval_identifier(other))
            }
            Resolved::NullInit => self.deopt(span, errs::PATH_WITHOUT_NODE),
            Resolved::DeclNode(type_name, _) => {
                self.deopt(span, errs::unsupported_expression(type_name))
            }
        }
    }

    fn eval_named_import(&mut self, symbol: SymbolId, name: &str, span: Span) -> EvalResult {
        if let Some(proxy) = self.state.theme_proxy(symbol) {
            return self.value(JsValue::proxy(proxy.clone()));
        }
        let Some(record) = self.state.imports.import_record(name).cloned() else {
            return self.deopt(span, errs::UNDEFINED_CONST);
        };
        let ImportedSymbol::Named(imported) = record.imported else {
            return self.deopt(span, errs::UNDEFINED_CONST);
        };
        match self.state.resolve_import_path(self.fs, &record.source) {
            Some(canonical) => {
                self.state.record_treeshake_import(
                    &record.source,
                    record.decl_span.start,
                    record.source_span,
                );
                let proxy = VarGroupProxy::new(canonical, imported, self.state.options);
                self.state.record_theme_proxy(symbol, proxy.clone());
                self.value(JsValue::proxy(proxy))
            }
            None => self.deopt(span, errs::IMPORT_PATH_RESOLUTION_ERROR),
        }
    }

    // parity: babel path.resolve() — follows declarator inits through
    // identifier chains with the `resolved` cycle stack and ancestor checks.
    fn resolve_symbol_decl(
        &mut self,
        symbol: SymbolId,
        visited: &mut Vec<(u32, u32)>,
    ) -> Resolved<'a> {
        let info = self.state.binding_info(symbol);
        let decl_key = (info.span.start, info.span.end);
        let terminal = |info: crate::state::BindingInfo<'a>| match info.decl {
            BindingDecl::Declarator(_) => Resolved::DeclNode("VariableDeclarator", info.span),
            BindingDecl::Opaque(type_name) => Resolved::DeclNode(type_name, info.span),
            // Imports never reach here (kind "module" stops resolve earlier).
            _ => Resolved::DeclNode("Identifier", info.span),
        };
        if visited.contains(&decl_key) {
            // babel: _resolve returns undefined on a revisit; the caller's
            // `|| this` lands back on the declaration node itself.
            return terminal(info);
        }
        visited.push(decl_key);
        match info.decl {
            BindingDecl::Declarator(declarator) => {
                if declarator.id.get_binding_identifier().is_none() {
                    return terminal(info);
                }
                if let Some(value) = self.state.binding_override(symbol) {
                    return Resolved::Override(value.clone());
                }
                match &declarator.init {
                    None => Resolved::NullInit,
                    Some(init) => self.resolve_expr(init, visited),
                }
            }
            _ => terminal(info),
        }
    }

    fn resolve_expr(
        &mut self,
        expr: &'a Expression<'a>,
        visited: &mut Vec<(u32, u32)>,
    ) -> Resolved<'a> {
        let span = expr.span();
        let key = (span.start, span.end);
        if visited.contains(&key) {
            return Resolved::Expr(expr);
        }
        visited.push(key);
        let Expression::Identifier(id) = expr else {
            return Resolved::Expr(expr);
        };
        let stop = Resolved::Identifier(id);
        let Some(symbol) = self.state.symbol_of(id) else {
            return stop;
        };
        // babel `binding.constant` counts violations only, not isMutated.
        if self.state.is_non_constant(symbol) {
            return stop;
        }
        let info = self.state.binding_info(symbol);
        if matches!(
            info.decl,
            BindingDecl::NamedImport | BindingDecl::DefaultImport | BindingDecl::NamespaceImport
        ) {
            return stop;
        }
        let resolved = self.resolve_symbol_decl(symbol, visited);
        // babel: a result that is (or contains) this reference fails this level.
        if let Some(rspan) = resolved.span()
            && rspan.start <= id.span.start
            && id.span.end <= rspan.end
        {
            return stop;
        }
        resolved
    }

    fn eval_member(&mut self, expr: &'a Expression<'a>) -> EvalResult {
        // parity: evaluate-path.js getFullMemberPath — a ≥2-part static chain
        // over a theme proxy resolves as one dotted key.
        if let Some((base, parts)) = collect_member_chain(expr)
            && parts.len() >= 2
        {
            let base_value = value_or_return!(self, base);
            if let JsValue::Proxy(proxy) = &base_value {
                return self.value(JsValue::Str(proxy.resolve_key(&parts.join("."))));
            }
            let mut current = base_value;
            for (i, key) in parts.iter().enumerate() {
                match self.member_lookup(&current, key, expr.span())? {
                    EvalOutcome::Value(v) => current = v,
                    deopt => return Ok(deopt),
                }
                if let JsValue::Proxy(proxy) = &current
                    && i + 1 < parts.len()
                {
                    let rest = parts[i + 1..].join(".");
                    return self.value(JsValue::Str(proxy.resolve_key(&rest)));
                }
            }
            return self.value(current);
        }
        let (object_expr, key): (_, Cow<str>) = match expr {
            Expression::StaticMemberExpression(member) => {
                (&member.object, Cow::Borrowed(member.property.name.as_str()))
            }
            Expression::ComputedMemberExpression(member) => {
                let key_value = value_or_return!(self, &member.expression);
                match js_to_string(&key_value) {
                    Some(key) => (&member.object, Cow::Owned(key)),
                    None => {
                        return self
                            .deopt(member.expression.span(), errs::UNEXPECTED_MEMBER_LOOKUP);
                    }
                }
            }
            _ => unreachable!("eval_member only receives member expressions"),
        };
        let object = value_or_return!(self, object_expr);
        self.member_lookup(&object, &key, expr.span())
    }

    fn member_lookup(&mut self, object: &JsValue, key: &str, span: Span) -> EvalResult {
        match object {
            JsValue::Proxy(proxy) => self.value(match key {
                "__IS_PROXY" => JsValue::Bool(true),
                "__varGroupHash__" => JsValue::Str(proxy.var_group_hash().to_string()),
                "toString" => JsValue::Callable(Callable::Opaque),
                _ => JsValue::Str(proxy.resolve_key(key)),
            }),
            JsValue::Obj(obj) => self.value(obj.get(key).cloned().unwrap_or(JsValue::Undefined)),
            JsValue::Arr(items) => self.value(match key {
                "length" => JsValue::Num(items.len() as f64),
                _ => array_index(key)
                    .and_then(|i| items.get(i as usize))
                    .cloned()
                    .unwrap_or(JsValue::Undefined),
            }),
            JsValue::Str(s) => {
                let value = match key {
                    "length" => JsValue::Num(s.encode_utf16().count() as f64),
                    _ => match array_index(key) {
                        Some(i) if (i as usize) < s.encode_utf16().count() => JsValue::Str(
                            crate::jsrt::js_slice_utf16_checked(s, i as usize, i as isize + 1)
                                .map_err(|_| StylexError::lone_surrogate("string indexing"))?,
                        ),
                        _ => JsValue::Undefined,
                    },
                };
                self.value(value)
            }
            JsValue::Num(_) | JsValue::Bool(_) | JsValue::Callable(_) => {
                self.value(JsValue::Undefined)
            }
            JsValue::Null | JsValue::Undefined => {
                let kind = if matches!(object, JsValue::Null) {
                    "null"
                } else {
                    "undefined"
                };
                let _ = span;
                // parity: upstream lets the raw TypeError escape the plugin.
                Err(StylexError::new(
                    ErrorCode::NonStaticValue,
                    format!("Cannot read properties of {kind} (reading '{key}')"),
                ))
            }
        }
    }

    fn eval_unary(&mut self, unary: &'a UnaryExpression<'a>) -> EvalResult {
        if unary.operator == UnaryOperator::Void {
            return self.value(JsValue::Undefined);
        }
        if unary.operator == UnaryOperator::Typeof
            && matches!(
                unary.argument,
                Expression::FunctionExpression(_)
                    | Expression::ArrowFunctionExpression(_)
                    | Expression::ClassExpression(_)
            )
        {
            return self.value(JsValue::Str("function".to_string()));
        }
        let arg = value_or_return!(self, &unary.argument);
        match unary.operator {
            UnaryOperator::LogicalNot => self.value(JsValue::Bool(!truthy(&arg))),
            UnaryOperator::UnaryPlus => self.value(JsValue::Num(js_to_number(&arg))),
            UnaryOperator::UnaryNegation => self.value(JsValue::Num(-js_to_number(&arg))),
            UnaryOperator::BitwiseNot => {
                self.value(JsValue::Num(f64::from(!to_int32(js_to_number(&arg)))))
            }
            UnaryOperator::Typeof => self.value(JsValue::Str(
                match arg {
                    JsValue::Undefined => "undefined",
                    JsValue::Null | JsValue::Arr(_) | JsValue::Obj(_) | JsValue::Proxy(_) => {
                        "object"
                    }
                    JsValue::Bool(_) => "boolean",
                    JsValue::Num(_) => "number",
                    JsValue::Str(_) => "string",
                    JsValue::Callable(_) => "function",
                }
                .to_string(),
            )),
            UnaryOperator::Void => unreachable!("handled above"),
            UnaryOperator::Delete => self.deopt(
                unary.span,
                errs::unsupported_operator(unary.operator.as_str()),
            ),
        }
    }

    fn eval_object(&mut self, object: &'a ObjectExpression<'a>) -> EvalResult {
        let mut obj = JsObj::with_capacity(object.properties.len());
        for property in &object.properties {
            match property {
                ObjectPropertyKind::SpreadProperty(spread) => {
                    let value = value_or_return!(self, &spread.argument);
                    spread_into(&mut obj, &value);
                }
                ObjectPropertyKind::ObjectProperty(prop) => {
                    if prop.method || prop.kind != PropertyKind::Init {
                        return self.deopt(prop.span, errs::OBJECT_METHOD);
                    }
                    let key = if prop.computed {
                        let value = value_or_return!(
                            self,
                            prop.key
                                .as_expression()
                                .expect("computed keys are expressions",)
                        );
                        match js_to_string(&value) {
                            Some(key) => key,
                            None => {
                                return self.deopt(prop.span, errs::UNEXPECTED_MEMBER_LOOKUP);
                            }
                        }
                    } else {
                        match static_property_key(&prop.key) {
                            Some(key) => key,
                            None => {
                                return self.deopt(prop.span, errs::UNEXPECTED_MEMBER_LOOKUP);
                            }
                        }
                    };
                    let value = value_or_return!(self, &prop.value);
                    obj.insert(key, value);
                }
            }
        }
        self.value(JsValue::object(obj))
    }

    // parity: evaluate-path.js isLogicalExpression — both sides evaluate with
    // fresh confidence; `0 ?? x` deopts with literally "unknown error".
    fn eval_logical(&mut self, logical: &'a LogicalExpression<'a>) -> EvalResult {
        let left = self.eval_node(&logical.left)?;
        let right = self.eval_node(&logical.right)?;
        if let EvalOutcome::Value(l) = &left {
            match logical.operator {
                LogicalOperator::Or => {
                    if truthy(l) {
                        return Ok(EvalOutcome::Value(l.clone()));
                    }
                    if let EvalOutcome::Value(r) = &right {
                        return Ok(EvalOutcome::Value(r.clone()));
                    }
                }
                LogicalOperator::And => {
                    if !truthy(l) {
                        return Ok(EvalOutcome::Value(l.clone()));
                    }
                    if let EvalOutcome::Value(r) = &right {
                        return Ok(EvalOutcome::Value(r.clone()));
                    }
                }
                LogicalOperator::Coalesce => {
                    if !is_nullish(l) {
                        if truthy(l) {
                            return Ok(EvalOutcome::Value(l.clone()));
                        }
                        // Non-nullish falsy left: upstream falls to "unknown error".
                    } else if let EvalOutcome::Value(r) = &right {
                        return Ok(EvalOutcome::Value(r.clone()));
                    }
                }
            }
        }
        match left {
            EvalOutcome::NonStatic(d) => Ok(EvalOutcome::NonStatic(d)),
            _ => match right {
                EvalOutcome::NonStatic(d) => Ok(EvalOutcome::NonStatic(d)),
                _ => self.deopt(logical.span, "unknown error"),
            },
        }
    }

    fn eval_binary(&mut self, binary: &'a BinaryExpression<'a>) -> EvalResult {
        let left = value_or_return!(self, &binary.left);
        let right = value_or_return!(self, &binary.right);
        let num =
            |f: fn(f64, f64) -> f64| JsValue::Num(f(js_to_number(&left), js_to_number(&right)));
        let int = |f: fn(i32, i32) -> i32| {
            JsValue::Num(f64::from(f(
                to_int32(js_to_number(&left)),
                to_int32(js_to_number(&right)),
            )))
        };
        let cmp = |wanted: std::cmp::Ordering, or_equal: bool| -> JsValue {
            JsValue::Bool(match js_compare(&left, &right) {
                Some(ordering) => {
                    ordering == wanted || (or_equal && ordering == std::cmp::Ordering::Equal)
                }
                None => false,
            })
        };
        use std::cmp::Ordering::{Greater, Less};
        let value = match binary.operator {
            BinaryOperator::Addition => match js_add(&left, &right) {
                Some(v) => v,
                None => {
                    return self.deopt(
                        binary.span,
                        errs::unsupported_expression("BinaryExpression"),
                    );
                }
            },
            BinaryOperator::Subtraction => num(|l, r| l - r),
            BinaryOperator::Multiplication => num(|l, r| l * r),
            BinaryOperator::Division => num(|l, r| l / r),
            BinaryOperator::Remainder => num(|l, r| l % r),
            BinaryOperator::Exponential => num(js_pow),
            BinaryOperator::LessThan => cmp(Less, false),
            BinaryOperator::LessEqualThan => cmp(Less, true),
            BinaryOperator::GreaterThan => cmp(Greater, false),
            BinaryOperator::GreaterEqualThan => cmp(Greater, true),
            BinaryOperator::Equality => JsValue::Bool(js_loose_eq(&left, &right)),
            // parity: evaluate-path.js:919 — `!=` is (mistakenly) strict upstream.
            BinaryOperator::Inequality => JsValue::Bool(!js_strict_eq(&left, &right)),
            BinaryOperator::StrictEquality => JsValue::Bool(js_strict_eq(&left, &right)),
            BinaryOperator::StrictInequality => JsValue::Bool(!js_strict_eq(&left, &right)),
            BinaryOperator::BitwiseOR => int(|l, r| l | r),
            BinaryOperator::BitwiseAnd => int(|l, r| l & r),
            BinaryOperator::BitwiseXOR => int(|l, r| l ^ r),
            BinaryOperator::ShiftLeft => int(|l, r| l.wrapping_shl(r as u32 & 31)),
            BinaryOperator::ShiftRight => int(|l, r| l.wrapping_shr(r as u32 & 31)),
            BinaryOperator::ShiftRightZeroFill => {
                let l = to_uint32(js_to_number(&left));
                let r = to_uint32(js_to_number(&right)) & 31;
                JsValue::Num(f64::from(l >> r))
            }
            BinaryOperator::In => {
                let key = match js_to_string(&left) {
                    Some(k) => k,
                    None => {
                        return self.deopt(
                            binary.span,
                            errs::unsupported_expression("BinaryExpression"),
                        );
                    }
                };
                match &right {
                    JsValue::Obj(obj) => JsValue::Bool(obj.get(&key).is_some()),
                    JsValue::Arr(items) => JsValue::Bool(
                        key == "length"
                            || array_index(&key).is_some_and(|i| (i as usize) < items.len()),
                    ),
                    JsValue::Proxy(_) => JsValue::Bool(false),
                    _ => {
                        return Err(StylexError::new(
                            ErrorCode::NonStaticValue,
                            format!(
                                "Cannot use 'in' operator to search for '{key}' in {}",
                                js_to_string(&right).unwrap_or_default()
                            ),
                        ));
                    }
                }
            }
            BinaryOperator::Instanceof => match &right {
                JsValue::Callable(_) => JsValue::Bool(false),
                _ => {
                    return Err(StylexError::new(
                        ErrorCode::NonStaticValue,
                        "Right-hand side of 'instanceof' is not callable",
                    ));
                }
            },
        };
        self.value(value)
    }

    fn eval_call(&mut self, call: &'a CallExpression<'a>) -> EvalResult {
        if call.optional {
            return self.deopt(
                call.span,
                errs::unsupported_expression("OptionalCallExpression"),
            );
        }
        let callee = unwrap_parens(&call.callee);
        if let Expression::Identifier(id) = callee {
            let name = id.name.as_str();
            // parity: `!path.scope.getBinding(name) && isValidCallee(name)`.
            if self.state.symbol_of(id).is_none() && methods::is_valid_callee(name) {
                let args = match self.eval_arguments_raw(call)? {
                    Ok(args) => args,
                    Err(deopt) => return Ok(EvalOutcome::NonStatic(deopt)),
                };
                return match methods::call_global_identifier(name, &args)? {
                    Some(value) => self.value(value),
                    None => self.deopt(call.span, errs::unsupported_expression("CallExpression")),
                };
            }
            if let Some(entry) = self.registry.identifiers.get(name).cloned() {
                return match entry {
                    RegistryEntry::Callable(callable) => self.dispatch(callable, call),
                    RegistryEntry::Value(value) => self.invoke_value(&value, call),
                };
            }
            return match self.eval_cached(id.span, |ev| ev.eval_identifier(id))? {
                EvalOutcome::Value(value) => self.invoke_value(&value, call),
                deopt => Ok(deopt),
            };
        }
        if let Some(member) = callee.as_member_expression() {
            // babel property extraction: static identifiers, computed
            // identifiers BY NAME, and computed string literals.
            let (prop_static, prop_string): (Option<&str>, Option<&str>) = match member {
                oxc_ast::ast::MemberExpression::StaticMemberExpression(m) => {
                    (Some(m.property.name.as_str()), None)
                }
                oxc_ast::ast::MemberExpression::ComputedMemberExpression(m) => {
                    match &m.expression {
                        Expression::Identifier(prop) => (Some(prop.name.as_str()), None),
                        Expression::StringLiteral(lit) => (None, Some(lit.value.as_str())),
                        _ => (None, None),
                    }
                }
                oxc_ast::ast::MemberExpression::PrivateFieldExpression(_) => (None, None),
            };
            if let Expression::Identifier(object) = member.object() {
                let object_name = object.name.as_str();
                if let Some(prop) = prop_static {
                    // parity: no binding check — a shadowed `Math` still hits
                    // the global for known methods.
                    if methods::is_valid_callee(object_name) && !methods::is_invalid_method(prop) {
                        match methods::lookup_global_static(object_name, prop) {
                            methods::StaticMember::Fn(found) => {
                                let args = match self.eval_arguments_raw(call)? {
                                    Ok(args) => args,
                                    Err(deopt) => return Ok(EvalOutcome::NonStatic(deopt)),
                                };
                                let result =
                                    methods::call_global_static(self, object_name, found, &args)?;
                                return self.value(result);
                            }
                            methods::StaticMember::NonCallable => {
                                return self.func_apply_error(call);
                            }
                            methods::StaticMember::Unsupported => {
                                return Err(StylexError::unsupported_api(&format!(
                                    "compile-time call to {object_name}.{prop}"
                                )));
                            }
                            methods::StaticMember::Unknown => {}
                        }
                    }
                }
                if let Some(prop) = prop_static.or(prop_string)
                    && let Some(callable) =
                        self.registry.member_callable(object_name, prop).cloned()
                {
                    return self.dispatch(callable, call);
                }
            }
            // parity: numeric-literal receivers bind no `this`; known Number
            // methods throw upstream before the value fallback runs.
            if let Expression::NumericLiteral(_) = unwrap_parens(member.object())
                && let Some(prop) = prop_static
                && let Some(known) = methods::NUM_METHODS
                    .iter()
                    .chain(methods::NUM_UNSUPPORTED.iter())
                    .find(|m| **m == prop)
            {
                if let Err(deopt) = self.eval_arguments_raw(call)? {
                    return Ok(EvalOutcome::NonStatic(deopt));
                }
                return Err(StylexError::new(
                    ErrorCode::NonStaticValue,
                    format!("Number.prototype.{known} requires that 'this' be a Number"),
                ));
            }
            let Some(property) = prop_static.or(prop_string).map(str::to_string) else {
                return self.deopt(call.span, errs::unsupported_expression("CallExpression"));
            };
            // parity: evaluate-path.js:1017-1031 — the object is evaluated in a
            // fresh state, so its deopt reason never escapes this branch.
            let object_value = match self.eval_fresh(member.object())? {
                EvalOutcome::Value(v) => v,
                EvalOutcome::NonStatic(_) => {
                    return self.deopt(call.span, errs::unsupported_expression("CallExpression"));
                }
            };
            if let JsValue::Proxy(proxy) = &object_value
                && property == "toString"
            {
                let hash = proxy.var_group_hash().to_string();
                return match self.eval_arguments(call)? {
                    Ok(_) => self.value(JsValue::Str(hash)),
                    Err(deopt) => Ok(EvalOutcome::NonStatic(deopt)),
                };
            }
            return self.invoke_member(&object_value, &property, call);
        }
        self.deopt(call.span, errs::unsupported_expression("CallExpression"))
    }

    // parity: the `if (func)` tail — arrows/callables run, truthy non-functions
    // throw `func.apply is not a function`, falsy values fall to the deopt.
    fn invoke_value(&mut self, value: &JsValue, call: &'a CallExpression<'a>) -> EvalResult {
        match value {
            JsValue::Callable(Callable::Stylex(callable)) => self.dispatch(callable.clone(), call),
            JsValue::Callable(Callable::Arrow(key)) => self.eval_arrow_call(*key, call),
            other if truthy(other) => self.func_apply_error(call),
            _ => self.deopt(call.span, errs::unsupported_expression("CallExpression")),
        }
    }

    fn func_apply_error(&mut self, call: &'a CallExpression<'a>) -> EvalResult {
        if let Err(deopt) = self.eval_arguments_raw(call)? {
            return Ok(EvalOutcome::NonStatic(deopt));
        }
        Err(StylexError::new(
            ErrorCode::NonStaticValue,
            "func.apply is not a function",
        ))
    }

    fn invoke_member(
        &mut self,
        object_value: &JsValue,
        property: &str,
        call: &'a CallExpression<'a>,
    ) -> EvalResult {
        // Own layer first (a JS property read), then the prototype tables.
        let own = match object_value {
            JsValue::Obj(obj) => obj.get(property).cloned(),
            _ => match self.member_lookup(object_value, property, call.span)? {
                EvalOutcome::Value(JsValue::Undefined) => None,
                EvalOutcome::Value(v) => Some(v),
                deopt => return Ok(deopt),
            },
        };
        if let Some(found) = own {
            return self.invoke_value(&found, call);
        }
        match methods::lookup_proto_method(object_value, property) {
            methods::ProtoLookup::Fn(found) => {
                let args = match self.eval_arguments_raw(call)? {
                    Ok(args) => args,
                    Err(deopt) => return Ok(EvalOutcome::NonStatic(deopt)),
                };
                let receiver = object_value.clone();
                let result = methods::call_proto_method(self, &receiver, found, &args)?;
                self.value(result)
            }
            methods::ProtoLookup::Unsupported(found) => Err(StylexError::unsupported_api(
                &format!("compile-time call to {found}"),
            )),
            methods::ProtoLookup::NotFound => {
                self.deopt(call.span, errs::unsupported_expression("CallExpression"))
            }
        }
    }

    // parity: the evaluate-path closure — args bind as identifiers, and a
    // non-confident body THROWS the deopt reason out of the whole transform.
    fn eval_arrow_call(&mut self, key: u32, call: &'a CallExpression<'a>) -> EvalResult {
        if !self.arrow_bodies.contains_key(&key) {
            return self.deopt(call.span, errs::unsupported_expression("CallExpression"));
        }
        let args = match self.eval_arguments_raw(call)? {
            Ok(args) => args,
            Err(deopt) => return Ok(EvalOutcome::NonStatic(deopt)),
        };
        let value = self.call_arrow_values(key, args)?;
        self.value(value)
    }

    /// The evaluatedFn closure body: args bind as registry identifiers over a
    /// fresh evaluate() call, and a non-confident body THROWS its reason.
    fn call_arrow_values(&mut self, key: u32, args: Vec<JsValue>) -> Result<JsValue, StylexError> {
        let Some((body, params)) = self.arrow_bodies.get(&key).map(|(b, p)| (*b, p.clone())) else {
            return Err(StylexError::new(
                ErrorCode::NonStaticValue,
                errs::unsupported_expression("CallExpression"),
            ));
        };
        let mut saved: Vec<(String, Option<RegistryEntry>)> = Vec::with_capacity(params.len());
        for (i, name) in params.iter().enumerate() {
            let value = args.get(i).cloned().unwrap_or(JsValue::Undefined);
            let previous = self
                .registry
                .to_mut()
                .identifiers
                .insert(name.clone(), RegistryEntry::Value(value));
            saved.push((name.clone(), previous));
        }
        let result = self.eval_fresh(body);
        let identifiers = &mut self.registry.to_mut().identifiers;
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
        match result? {
            EvalOutcome::Value(v) => Ok(v),
            EvalOutcome::NonStatic(d) => Err(StylexError::new(ErrorCode::NonStaticValue, d.reason)),
        }
    }

    fn dispatch(&mut self, callable: StylexCallable, call: &'a CallExpression<'a>) -> EvalResult {
        let raw = match self.eval_arguments_raw(call)? {
            Ok(args) => args,
            Err(deopt) => return Ok(EvalOutcome::NonStatic(deopt)),
        };
        // parity: when.js fromProxy — only the when.* fns read a theme proxy,
        // via its toString() (the var-group hash).
        let is_when = matches!(
            callable,
            StylexCallable::WhenAncestor
                | StylexCallable::WhenDescendant
                | StylexCallable::WhenSiblingBefore
                | StylexCallable::WhenSiblingAfter
                | StylexCallable::WhenAnySibling
        );
        let args: Vec<EvalValue> = raw
            .iter()
            .map(|value| match value {
                JsValue::Proxy(proxy) if is_when => {
                    EvalValue::Str(proxy.var_group_hash().to_string())
                }
                other => to_eval_value(other),
            })
            .collect();
        let result = self.callables.call(&callable, &args, self.state)?;
        self.value(into_js_value(result))
    }

    fn eval_arguments(
        &mut self,
        call: &'a CallExpression<'a>,
    ) -> Result<Result<Vec<EvalValue>, Deopt>, StylexError> {
        Ok(self
            .eval_arguments_raw(call)?
            .map(|args| args.iter().map(to_eval_value).collect()))
    }

    fn eval_arguments_raw(
        &mut self,
        call: &'a CallExpression<'a>,
    ) -> Result<Result<Vec<JsValue>, Deopt>, StylexError> {
        let mut out = Vec::with_capacity(call.arguments.len());
        for argument in &call.arguments {
            match argument {
                Argument::SpreadElement(spread) => {
                    return Ok(Err(Deopt {
                        reason: errs::unsupported_expression("SpreadElement").into(),
                        span: spread.span,
                    }));
                }
                _ => match self.eval_node(argument.to_expression())? {
                    EvalOutcome::Value(v) => out.push(v),
                    EvalOutcome::NonStatic(deopt) => return Ok(Err(deopt)),
                },
            }
        }
        Ok(Ok(out))
    }
}

impl ArrowCaller for Evaluator<'_, '_> {
    fn call_arrow(&mut self, key: u32, args: Vec<JsValue>) -> Result<JsValue, StylexError> {
        self.call_arrow_values(key, args)
    }
}

#[derive(Debug)]
pub struct EvaluatedCreateArg {
    pub namespaces: JsObjectMap,
    /// Source byte offset of each namespace property (first occurrence wins);
    /// the visitor derives `CreateContext.namespace_lines` from these.
    pub key_spans: Vec<(String, u32)>,
    /// Arrow namespaces (`fns` upstream): namespace → params + inline styles.
    pub fns: Vec<(String, DynamicFn)>,
}

/// Evaluates one `stylex.create` argument into namespace objects.
// parity: parse-stylex-create-arg.js evaluateStyleXCreateArg (static namespaces)
pub fn evaluate_stylex_create_arg<'a>(
    evaluator: &mut Evaluator<'a, '_>,
    object: &'a ObjectExpression<'a>,
) -> Result<EvaluatedCreateArg, StylexError> {
    let _t = crate::timings::start(crate::timings::Stage::Eval);
    let mut namespaces = JsObjectMap::with_capacity(object.properties.len());
    let mut key_spans: Vec<(String, u32)> = Vec::new();
    let mut fns: Vec<(String, DynamicFn)> = Vec::new();
    for property in &object.properties {
        let prop = match property {
            ObjectPropertyKind::ObjectProperty(prop)
                if !prop.method && prop.kind == PropertyKind::Init =>
            {
                prop
            }
            // parity: non-plain properties re-evaluate the whole argument.
            _ => return reevaluate_create_object(evaluator, object),
        };
        let key = if prop.computed {
            let key_expr = prop
                .key
                .as_expression()
                .expect("computed keys are expressions");
            match evaluator.eval_entry(key_expr)? {
                EvalOutcome::Value(v) => match js_to_string(&v) {
                    Some(key) => key,
                    None => return Err(StylexError::non_static_value("create")),
                },
                // parity: evaluateObjKey drops the deopt reason for namespace keys.
                EvalOutcome::NonStatic(_) => {
                    return Err(StylexError::non_static_value("create"));
                }
            }
        } else {
            static_property_key(&prop.key).ok_or_else(|| StylexError::non_static_value("create"))?
        };
        if let Expression::ArrowFunctionExpression(arrow) = unwrap_parens(&prop.value) {
            if !evaluator.dynamic_namespaces {
                return Err(StylexError::unsupported_api(
                    "dynamic styles (arrow-function namespaces)",
                ));
            }
            // Defaulted params live in `initializer` (babel: AssignmentPattern).
            let params: Option<Vec<String>> = if arrow.params.rest.is_some() {
                None
            } else {
                arrow
                    .params
                    .items
                    .iter()
                    .map(|p| match &p.pattern {
                        oxc_ast::ast::BindingPattern::BindingIdentifier(id)
                            if p.initializer.is_none() =>
                        {
                            Some(id.name.to_string())
                        }
                        _ => None,
                    })
                    .collect()
            };
            let Some(params) = params else {
                return Err(StylexError::only_named_parameters());
            };
            // parity: only expression bodies that are object literals compile;
            // everything else re-evaluates the whole argument.
            let body = arrow.body.as_expression().map(unwrap_parens);
            let Some(Expression::ObjectExpression(body_obj)) = body else {
                return reevaluate_create_object(evaluator, object);
            };
            let mut fn_def = DynamicFn {
                params,
                inline_styles: Vec::new(),
            };
            let partial = evaluate_partial_object(evaluator, body_obj, &[], &mut fn_def);
            let Some(value) = partial? else {
                // parity: the arrow branch drops every partial-eval deopt reason.
                return Err(StylexError::non_static_value("create"));
            };
            if !key_spans.iter().any(|(k, _)| *k == key) {
                key_spans.push((key.clone(), prop.span.start));
            }
            namespaces.insert(key.clone(), evaluator.consume(JsValue::object(value)));
            // parity: `fns[key] =` is a [[Set]]; "__proto__" never lands.
            if key != "__proto__" {
                match fns.iter_mut().find(|(k, _)| *k == key) {
                    Some(slot) => slot.1 = fn_def,
                    None => fns.push((key, fn_def)),
                }
            }
            continue;
        }
        match evaluator.eval_entry(&prop.value)? {
            EvalOutcome::Value(v) => {
                if !key_spans.iter().any(|(k, _)| *k == key) {
                    key_spans.push((key.clone(), prop.span.start));
                }
                namespaces.insert(key, evaluator.consume(v));
            }
            EvalOutcome::NonStatic(deopt) => {
                return Err(StylexError::new(ErrorCode::NonStaticValue, deopt.reason));
            }
        }
    }
    Ok(EvaluatedCreateArg {
        namespaces,
        key_spans,
        fns,
    })
}

// parity: `return evaluate(path, …)` over the whole create argument.
fn reevaluate_create_object<'a>(
    evaluator: &mut Evaluator<'a, '_>,
    object: &'a ObjectExpression<'a>,
) -> Result<EvaluatedCreateArg, StylexError> {
    evaluator.seen.clear();
    match evaluator.eval_object(object)? {
        EvalOutcome::Value(v) => Ok(EvaluatedCreateArg {
            namespaces: match evaluator.consume(v) {
                EvalValue::Obj(map) => Arc::unwrap_or_clone(map),
                _ => JsObjectMap::new(),
            },
            key_spans: Vec::new(),
            fns: Vec::new(),
        }),
        EvalOutcome::NonStatic(deopt) => {
            Err(StylexError::new(ErrorCode::NonStaticValue, deopt.reason))
        }
    }
}

/// `None` = non-confident: the caller reports the reasonless create error.
// parity: parse-stylex-create-arg.js evaluatePartialObjectRecursively
fn evaluate_partial_object<'a>(
    evaluator: &mut Evaluator<'a, '_>,
    object: &'a ObjectExpression<'a>,
    key_path: &[String],
    fn_def: &mut DynamicFn,
) -> Result<Option<JsObj>, StylexError> {
    let mut obj = JsObj::with_capacity(object.properties.len());
    for property in &object.properties {
        match property {
            ObjectPropertyKind::SpreadProperty(spread) => {
                match evaluator.eval_entry(&spread.argument)? {
                    EvalOutcome::Value(v) => spread_into(&mut obj, &v),
                    EvalOutcome::NonStatic(_) => return Ok(None),
                }
            }
            ObjectPropertyKind::ObjectProperty(prop) => {
                if prop.method || prop.kind != PropertyKind::Init {
                    return Ok(None);
                }
                let mut key = if prop.computed {
                    let key_expr = prop
                        .key
                        .as_expression()
                        .expect("computed keys are expressions");
                    match evaluator.eval_entry(key_expr)? {
                        EvalOutcome::Value(v) => match js_to_string(&v) {
                            Some(key) => key,
                            None => return Ok(None),
                        },
                        EvalOutcome::NonStatic(_) => return Ok(None),
                    }
                } else {
                    match static_property_key(&prop.key) {
                        Some(key) => key,
                        None => return Ok(None),
                    }
                };
                // defineConsts at-rule placeholders stay wrapped past the root.
                if key_path.is_empty() && key.starts_with("var(") && key.ends_with(')') {
                    key = js_slice_utf16(&key, 4, -1);
                }
                if let Expression::ObjectExpression(inner) = unwrap_parens(&prop.value) {
                    let mut nested = key_path.to_vec();
                    nested.push(key.clone());
                    match evaluate_partial_object(evaluator, inner, &nested, fn_def)? {
                        Some(inner_obj) => obj.insert(key, JsValue::object(inner_obj)),
                        None => return Ok(None),
                    }
                } else {
                    match evaluator.eval_entry(&prop.value)? {
                        EvalOutcome::Value(v) => obj.insert(key, v),
                        EvalOutcome::NonStatic(_) => {
                            let (var_name, style) =
                                inline_style_for_leaf(&prop.value, key_path, &key);
                            obj.insert(key, JsValue::Str(format!("var({var_name})")));
                            fn_def.insert_inline(var_name, style);
                        }
                    }
                }
            }
        }
    }
    Ok(Some(obj))
}

// parity: stylex-create.js validateStyleXCreate (arg count / object / spread).
pub fn validate_create_arg<'a, 'b>(
    call: &'b CallExpression<'a>,
) -> Result<&'b ObjectExpression<'a>, StylexError> {
    if call.arguments.len() != 1 {
        return Err(StylexError::illegal_argument_length("create", 1));
    }
    let arg = call.arguments[0]
        .as_expression()
        .map(unwrap_parens)
        .ok_or_else(|| StylexError::non_style_object("create"))?;
    let Expression::ObjectExpression(object) = arg else {
        return Err(StylexError::non_style_object("create"));
    };
    if object
        .properties
        .iter()
        .any(|p| matches!(p, ObjectPropertyKind::SpreadProperty(_)))
    {
        return Err(StylexError::no_object_spreads());
    }
    Ok(object)
}

pub fn unwrap_parens<'a, 'b>(expr: &'b Expression<'a>) -> &'b Expression<'a> {
    match expr {
        Expression::ParenthesizedExpression(e) => unwrap_parens(&e.expression),
        _ => expr,
    }
}

fn collect_member_chain<'a, 'b>(
    expr: &'b Expression<'a>,
) -> Option<(&'b Expression<'a>, Vec<Cow<'b, str>>)> {
    let mut depth = 0;
    let mut current = expr;
    loop {
        match current {
            Expression::StaticMemberExpression(member) => current = &member.object,
            Expression::ComputedMemberExpression(member) => match &member.expression {
                Expression::StringLiteral(_) | Expression::NumericLiteral(_) => {
                    current = &member.object;
                }
                _ => return None,
            },
            _ => break,
        }
        depth += 1;
    }
    if depth < 2 {
        return None;
    }
    let mut parts: Vec<Cow<'b, str>> = Vec::with_capacity(depth);
    let mut current = expr;
    loop {
        match current {
            Expression::StaticMemberExpression(member) => {
                parts.push(Cow::Borrowed(member.property.name.as_str()));
                current = &member.object;
            }
            Expression::ComputedMemberExpression(member) => {
                match &member.expression {
                    Expression::StringLiteral(lit) => {
                        parts.push(Cow::Borrowed(lit.value.as_str()));
                    }
                    Expression::NumericLiteral(lit) => {
                        parts.push(Cow::Owned(js_number_to_string(lit.value)));
                    }
                    _ => unreachable!("checked by the depth pass"),
                }
                current = &member.object;
            }
            _ => break,
        }
    }
    parts.reverse();
    Some((current, parts))
}

fn static_property_key(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(id) => Some(id.name.to_string()),
        PropertyKey::StringLiteral(lit) => Some(lit.value.to_string()),
        PropertyKey::NumericLiteral(lit) => Some(js_number_to_string(lit.value)),
        _ => None,
    }
}

// parity: Object.assign source-iteration; non-object primitives are no-ops.
fn spread_into(target: &mut JsObj, value: &JsValue) {
    match value {
        JsValue::Obj(obj) => {
            for (k, v) in obj.entries() {
                target.insert(k.to_string(), v.clone());
            }
        }
        JsValue::Arr(items) => {
            for (i, item) in items.iter().enumerate() {
                target.insert(i.to_string(), item.clone());
            }
        }
        JsValue::Str(s) => {
            let units = s.encode_utf16().count();
            for i in 0..units {
                let unit = js_slice_utf16_checked(s, i, i as isize + 1)
                    .unwrap_or_else(|_| '\u{FFFD}'.to_string());
                target.insert(i.to_string(), JsValue::Str(unit));
            }
        }
        _ => {}
    }
}

fn babel_type_name(expr: &Expression<'_>) -> &'static str {
    match expr {
        Expression::BigIntLiteral(_) => "BigIntLiteral",
        Expression::RegExpLiteral(_) => "RegExpLiteral",
        Expression::FunctionExpression(_) => "FunctionExpression",
        Expression::ClassExpression(_) => "ClassExpression",
        Expression::AssignmentExpression(_) => "AssignmentExpression",
        Expression::AwaitExpression(_) => "AwaitExpression",
        Expression::NewExpression(_) => "NewExpression",
        Expression::TaggedTemplateExpression(_) => "TaggedTemplateExpression",
        Expression::ThisExpression(_) => "ThisExpression",
        Expression::UpdateExpression(_) => "UpdateExpression",
        Expression::YieldExpression(_) => "YieldExpression",
        Expression::ImportExpression(_) | Expression::CallExpression(_) => "CallExpression",
        Expression::ImportMeta(_) | Expression::NewTarget(_) => "MetaProperty",
        Expression::JSXElement(_) => "JSXElement",
        Expression::JSXFragment(_) => "JSXFragment",
        Expression::TSTypeAssertion(_) => "TSTypeAssertion",
        Expression::TSNonNullExpression(_) => "TSNonNullExpression",
        Expression::TSInstantiationExpression(_) => "TSInstantiationExpression",
        Expression::PrivateInExpression(_) => "BinaryExpression",
        Expression::ArrowFunctionExpression(_) => "ArrowFunctionExpression",
        Expression::SequenceExpression(_) => "SequenceExpression",
        Expression::Identifier(_) => "Identifier",
        Expression::Super(_) => "Super",
        Expression::PrivateFieldExpression(_) => "MemberExpression",
        _ => "Expression",
    }
}

const VALID_CALLEES: [&str; 5] = ["String", "Number", "Math", "Object", "Array"];
const INVALID_METHODS: [&str; 7] = [
    "random",
    "assign",
    "defineProperties",
    "defineProperty",
    "freeze",
    "seal",
    "splice",
];

fn is_valid_callee(name: &str) -> bool {
    VALID_CALLEES.contains(&name)
}

fn is_invalid_method(name: &str) -> bool {
    INVALID_METHODS.contains(&name)
}

pub fn truthy(value: &JsValue) -> bool {
    match value {
        JsValue::Null | JsValue::Undefined => false,
        JsValue::Bool(b) => *b,
        JsValue::Num(n) => *n != 0.0 && !n.is_nan(),
        JsValue::Str(s) => !s.is_empty(),
        _ => true,
    }
}

pub fn is_nullish(value: &JsValue) -> bool {
    matches!(value, JsValue::Null | JsValue::Undefined)
}

/// JS ToString; `None` for function values (their source text is unmodelled).
pub fn js_to_string(value: &JsValue) -> Option<String> {
    Some(match value {
        JsValue::Null => "null".to_string(),
        JsValue::Undefined => "undefined".to_string(),
        JsValue::Bool(b) => b.to_string(),
        JsValue::Num(n) => js_number_to_string(*n),
        JsValue::Str(s) => s.clone(),
        JsValue::Arr(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|item| {
                    if is_nullish(item) {
                        Some(String::new())
                    } else {
                        js_to_string(item)
                    }
                })
                .collect::<Option<_>>()?;
            parts.join(",")
        }
        JsValue::Obj(_) => "[object Object]".to_string(),
        JsValue::Proxy(proxy) => proxy.var_group_hash().to_string(),
        JsValue::Callable(_) => return None,
    })
}

pub fn js_to_number(value: &JsValue) -> f64 {
    match value {
        JsValue::Null => 0.0,
        JsValue::Undefined | JsValue::Callable(_) => f64::NAN,
        JsValue::Bool(b) => f64::from(*b as u8),
        JsValue::Num(n) => *n,
        JsValue::Str(s) => string_to_number(s),
        other => js_to_string(other).map_or(f64::NAN, |s| string_to_number(&s)),
    }
}

// parity: ES StringToNumber (TrimString; hex/octal/binary; empty string is 0).
pub fn string_to_number(s: &str) -> f64 {
    let trimmed: &str = crate::jsrt::js_trim(s);
    if trimmed.is_empty() {
        return 0.0;
    }
    if let Some(rest) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return u64::from_str_radix(rest, 16).map_or(f64::NAN, |n| n as f64);
    }
    if let Some(rest) = trimmed
        .strip_prefix("0o")
        .or_else(|| trimmed.strip_prefix("0O"))
    {
        return u64::from_str_radix(rest, 8).map_or(f64::NAN, |n| n as f64);
    }
    if let Some(rest) = trimmed
        .strip_prefix("0b")
        .or_else(|| trimmed.strip_prefix("0B"))
    {
        return u64::from_str_radix(rest, 2).map_or(f64::NAN, |n| n as f64);
    }
    match trimmed {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    if !trimmed
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'))
    {
        return f64::NAN;
    }
    trimmed.parse::<f64>().unwrap_or(f64::NAN)
}

fn js_add(left: &JsValue, right: &JsValue) -> Option<JsValue> {
    let string_side = |v: &JsValue| {
        matches!(
            v,
            JsValue::Str(_) | JsValue::Arr(_) | JsValue::Obj(_) | JsValue::Proxy(_)
        )
    };
    if string_side(left) || string_side(right) {
        Some(JsValue::Str(format!(
            "{}{}",
            js_to_string(left)?,
            js_to_string(right)?
        )))
    } else if matches!(left, JsValue::Callable(_)) || matches!(right, JsValue::Callable(_)) {
        None
    } else {
        Some(JsValue::Num(js_to_number(left) + js_to_number(right)))
    }
}

// parity: JS `**` — |1| ** ±Infinity is NaN where Rust powf returns 1.
pub(crate) fn js_pow(base: f64, exponent: f64) -> f64 {
    if exponent.is_infinite() && base.abs() == 1.0 {
        return f64::NAN;
    }
    base.powf(exponent)
}

fn is_object_like(value: &JsValue) -> bool {
    matches!(
        value,
        JsValue::Obj(_) | JsValue::Arr(_) | JsValue::Proxy(_) | JsValue::Callable(_)
    )
}

/// ES OrdinaryToPrimitive for the evaluator's object values: plain objects
/// and arrays have no useful valueOf, so both hints reach toString.
fn js_to_primitive(value: &JsValue) -> Option<JsValue> {
    if is_object_like(value) {
        js_to_string(value).map(JsValue::Str)
    } else {
        Some(value.clone())
    }
}

// parity: ES Abstract Relational Comparison (ToPrimitive first; strings
// compare by UTF-16 code units, everything else through ToNumber).
fn js_compare(left: &JsValue, right: &JsValue) -> Option<std::cmp::Ordering> {
    let (left, right) = (js_to_primitive(left)?, js_to_primitive(right)?);
    if let (JsValue::Str(l), JsValue::Str(r)) = (&left, &right) {
        return Some(utf16_cmp(l, r));
    }
    let l = js_to_number(&left);
    let r = js_to_number(&right);
    l.partial_cmp(&r)
}

pub(crate) fn js_strict_eq(left: &JsValue, right: &JsValue) -> bool {
    match (left, right) {
        (JsValue::Null, JsValue::Null) | (JsValue::Undefined, JsValue::Undefined) => true,
        (JsValue::Bool(l), JsValue::Bool(r)) => l == r,
        (JsValue::Num(l), JsValue::Num(r)) => l == r,
        (JsValue::Str(l), JsValue::Str(r)) => l == r,
        (JsValue::Obj(l), JsValue::Obj(r)) => Rc::ptr_eq(l, r),
        (JsValue::Arr(l), JsValue::Arr(r)) => Rc::ptr_eq(l, r),
        (JsValue::Proxy(l), JsValue::Proxy(r)) => Rc::ptr_eq(l, r),
        // A cached arrow closure keeps its node key; two closures from the
        // same arrow in one evaluation are the same function upstream.
        (JsValue::Callable(Callable::Arrow(l)), JsValue::Callable(Callable::Arrow(r))) => l == r,
        _ => false,
    }
}

// parity: ES Abstract Equality Comparison.
fn js_loose_eq(left: &JsValue, right: &JsValue) -> bool {
    match (left, right) {
        (JsValue::Null | JsValue::Undefined, JsValue::Null | JsValue::Undefined) => true,
        (JsValue::Null | JsValue::Undefined, _) | (_, JsValue::Null | JsValue::Undefined) => false,
        _ if is_object_like(left) && is_object_like(right) => js_strict_eq(left, right),
        _ if is_object_like(left) => match js_to_primitive(left) {
            Some(prim) => js_loose_eq(&prim, right),
            None => false,
        },
        _ if is_object_like(right) => match js_to_primitive(right) {
            Some(prim) => js_loose_eq(left, &prim),
            None => false,
        },
        (JsValue::Str(l), JsValue::Str(r)) => l == r,
        _ => js_to_number(left) == js_to_number(right),
    }
}

// parity: ES ToInt32/ToUint32.
pub(crate) fn to_int32(n: f64) -> i32 {
    to_uint32(n) as i32
}

pub(crate) fn to_uint32(n: f64) -> u32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    let modulus = 2f64.powi(32);
    let mut m = n.trunc() % modulus;
    if m < 0.0 {
        m += modulus;
    }
    m as u32
}

#[cfg(test)]
mod js_obj_tests {
    use super::*;

    #[test]
    fn wide_objects_keep_insertion_order_and_overwrite_position() {
        let n = value::NAMED_INDEX_THRESHOLD * 3;
        let mut obj = JsObj::default();
        for i in 0..n {
            obj.insert(format!("key{i}"), JsValue::Num(i as f64));
        }
        obj.insert("key5".to_string(), JsValue::Str("updated".to_string()));
        obj.insert("late".to_string(), JsValue::Bool(true));
        let keys: Vec<&str> = obj.entries().map(|(k, _)| k).collect();
        let mut expected: Vec<String> = (0..n).map(|i| format!("key{i}")).collect();
        expected.push("late".to_string());
        assert_eq!(keys, expected);
        assert!(matches!(obj.get("key5"), Some(JsValue::Str(s)) if s == "updated"));
        assert!(obj.get("missing").is_none());
    }
}

#[cfg(test)]
mod coercion_lattice_tests {
    use super::*;

    fn grid_value(index: usize) -> JsValue {
        // Positionally mirrors VALUES in conformance/src/gen-pins-coercion.mjs.
        let n = JsValue::Num;
        let s = |v: &str| JsValue::Str(v.to_string());
        match index {
            0 => JsValue::Undefined,
            1 => JsValue::Null,
            2 => JsValue::Bool(true),
            3 => JsValue::Bool(false),
            4 => n(0.0),
            5 => n(-0.0),
            6 => n(1.0),
            7 => n(-1.0),
            8 => n(0.5),
            9 => n(f64::NAN),
            10 => n(f64::INFINITY),
            11 => n(f64::NEG_INFINITY),
            12 => s(""),
            13 => s("0"),
            14 => s("00"),
            15 => s("1"),
            16 => s("2"),
            17 => s("10"),
            18 => s("a"),
            19 => s(" "),
            20 => s("\u{0085}"),
            21 => s("Infinity"),
            22 => JsValue::array(vec![]),
            23 => JsValue::array(vec![n(0.0)]),
            24 => JsValue::array(vec![n(1.0)]),
            25 => JsValue::array(vec![JsValue::array(vec![])]),
            26 => JsValue::array(vec![s("2")]),
            27 => JsValue::array(vec![s("10")]),
            28 => JsValue::array(vec![JsValue::Null]),
            29 => JsValue::array(vec![JsValue::Undefined]),
            30 => JsValue::array(vec![n(1.0), n(2.0)]),
            31 => JsValue::array(vec![s("a"), s("b")]),
            32 => JsValue::object(JsObj::default()),
            33 => {
                let mut obj = JsObj::default();
                obj.insert("a".to_string(), n(1.0));
                JsValue::object(obj)
            }
            _ => unreachable!("grid index"),
        }
    }

    #[test]
    fn lattice_matches_node_pins() {
        // Vendored copies of this crate ship without testdata; the pin gate
        // runs wherever the conformance harness lives.
        let Ok(raw) = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/pins/eval/coercion.json"
        )) else {
            eprintln!("skipping lattice_matches_node_pins: testdata not vendored");
            return;
        };
        let pins: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let values = pins["values"].as_array().unwrap();
        assert_eq!(values.len(), 34, "grid size drifted; update grid_value");
        let mut checked = 0;
        for row in pins["results"].as_array().unwrap() {
            let i = row["i"].as_u64().unwrap() as usize;
            let j = row["j"].as_u64().unwrap() as usize;
            let label = |op: &str| format!("{} {op} {}", values[i], values[j]);
            let (left, right) = (grid_value(i), grid_value(j));
            assert_eq!(
                js_loose_eq(&left, &right),
                row["=="].as_bool().unwrap(),
                "{}",
                label("==")
            );
            assert_eq!(
                js_strict_eq(&left, &right),
                row["==="].as_bool().unwrap(),
                "{}",
                label("===")
            );
            use std::cmp::Ordering::{Equal, Greater, Less};
            let cmp = js_compare(&left, &right);
            assert_eq!(
                cmp == Some(Less),
                row["<"].as_bool().unwrap(),
                "{}",
                label("<")
            );
            assert_eq!(
                matches!(cmp, Some(Less | Equal)),
                row["<="].as_bool().unwrap(),
                "{}",
                label("<=")
            );
            assert_eq!(
                cmp == Some(Greater),
                row[">"].as_bool().unwrap(),
                "{}",
                label(">")
            );
            assert_eq!(
                matches!(cmp, Some(Greater | Equal)),
                row[">="].as_bool().unwrap(),
                "{}",
                label(">=")
            );
            let sum = js_add(&left, &right).expect("grid holds no function values");
            let (tag, expected) = (
                row["add"]["t"].as_str().unwrap(),
                row["add"]["v"].as_str().unwrap(),
            );
            match (tag, &sum) {
                ("num", JsValue::Num(x)) => {
                    assert_eq!(js_number_to_string(*x), expected, "{}", label("+"));
                }
                ("str", JsValue::Str(x)) => assert_eq!(x, expected, "{}", label("+")),
                other => panic!("{}: unexpected sum shape {other:?}", label("+")),
            }
            checked += 1;
        }
        assert_eq!(checked, 34 * 34);
    }
}
