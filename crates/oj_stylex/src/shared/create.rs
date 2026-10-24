//! `stylex.create` namespace compilation over already-evaluated style objects.
// parity: babel-plugin src/shared/stylex-create.js + visitors/stylex-create.js:252-283

use crate::errors::StylexError;
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::fxhash::FxHashMap;
use crate::fxhash::FxHashSet;
use crate::hash::create_short_hash;
use crate::options::{ModuleResolutionType, ResolvedOptions};
use crate::rules::StylexRule;
use crate::shared::dashify::sanitize_dev_class_name;
use crate::shared::dev_naming::{
    DebugPathInfo, convert_to_test_styles, create_short_filename, dev_class_prefix,
};
use crate::shared::flatten::{
    PreRule, PreRuleValue, StyleScalar, flatten_raw_style_object, media_order_transform,
    validate_namespace,
};
use crate::shared::generate_rule::CompiledDecl;
use crate::shared::resolution::is_unexpanded_property;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Per-call inputs the babel visitor reads from the AST / process state.
/// Wave 3 (AST front end) fills these; pins feed them synthetically.
#[derive(Debug, Clone)]
pub struct CreateContext<'a> {
    pub options: &'a ResolvedOptions,
    /// Absolute source filename ('/'-separated); `None` only in AST-less tests.
    pub filename: Option<String>,
    /// process.cwd() equivalent; unused until wave 3 resolves packages itself.
    pub cwd: String,
    /// Name of the variable the create() result is assigned to, if any.
    pub var_name: Option<String>,
    /// 1-based source line of each namespace property (babel `loc.start.line`).
    pub namespace_lines: Option<BTreeMap<String, u32>>,
    /// Nearest package.json walking up from dirname(filename): (name, dir).
    pub file_package: Option<(String, String)>,
    /// Name of the nearest package.json walking up from dirname(cwd).
    pub cwd_package_name: Option<String>,
}

impl<'a> CreateContext<'a> {
    pub fn new(options: &'a ResolvedOptions) -> Self {
        CreateContext {
            options,
            filename: None,
            cwd: String::new(),
            var_name: None,
            namespace_lines: None,
            file_package: None,
            cwd_package_name: None,
        }
    }
}

/// className → keyPath, first-seen order with last-write-wins values.
pub type ClassPathsInNamespace = Vec<(String, Vec<String>)>;

#[derive(Debug, Clone, PartialEq)]
pub struct CreateOutput {
    /// namespace name → compiled object (className strings / null, `$$css`).
    pub compiled: Arc<JsObjectMap>,
    /// Injected rules in traversal order, deduped by className (first wins).
    pub rules: Vec<StylexRule>,
    /// namespace → className → keyPath (classPathsPerNamespace upstream);
    /// built only when the caller has dynamic-fn namespaces to rewrite.
    pub class_paths: Vec<(String, ClassPathsInNamespace)>,
}

/// The namespace an atom compiles under (`@stylexjs/atoms` styleInput key).
pub const INLINE_NAMESPACE: &str = "__inline__";

/// parity: atoms babel-transform compileStaticStyle — styleXCreateSet plus the
/// dev-class step only, never source-map data and never test styles.
pub fn compile_atom(
    property: &str,
    value: &str,
    ctx: &CreateContext<'_>,
) -> Result<CreateOutput, StylexError> {
    let mut raw = JsObjectMap::new();
    raw.insert(property, EvalValue::Str(value.to_string()));
    let mut namespaces = JsObjectMap::new();
    namespaces.insert(INLINE_NAMESPACE, EvalValue::Obj(raw.into()));
    let mut shape = DevShape {
        dev_prefix: dev_prefix_for(ctx, None),
        debug: None,
    };
    compile_namespaces_core(&namespaces, ctx, false, &mut shape)
}

/// Dynamic-fn namespaces are wave 3: callers reject them with `unsupported_api`
/// before this point (`EvalValue` cannot represent functions).
pub fn compile_namespaces(
    namespaces: &JsObjectMap,
    ctx: &CreateContext<'_>,
    need_class_paths: bool,
) -> Result<CreateOutput, StylexError> {
    let options = ctx.options;
    let debug = if options.debug && options.enable_debug_data_prop {
        ctx.namespace_lines
            .as_ref()
            .zip(ctx.filename.as_deref())
            .map(|(lines, filename)| DebugSource {
                lines,
                filename,
                info: DebugPathInfo {
                    file_package: ctx.file_package.clone(),
                    cwd_package_name: ctx.cwd_package_name.clone(),
                    root_dir: options
                        .unstable_module_resolution
                        .as_ref()
                        .and_then(|m| m.root_dir.as_ref())
                        .map(|r| r.to_string_lossy().into_owned()),
                    is_haste: options
                        .unstable_module_resolution
                        .as_ref()
                        .is_some_and(|m| m.kind == ModuleResolutionType::Haste),
                },
                short: None,
            })
    } else {
        None
    };
    let mut shape = DevShape {
        dev_prefix: dev_prefix_for(ctx, ctx.var_name.as_deref()),
        debug,
    };
    let mut out = compile_namespaces_core(namespaces, ctx, need_class_paths, &mut shape)?;
    if options.test {
        out.compiled = convert_to_test_styles(
            &out.compiled,
            ctx.var_name.as_deref(),
            ctx.filename.as_deref(),
        );
    }
    Ok(out)
}

struct DevShape<'a> {
    dev_prefix: Option<String>,
    debug: Option<DebugSource<'a>>,
}

struct DebugSource<'a> {
    lines: &'a BTreeMap<String, u32>,
    filename: &'a str,
    info: DebugPathInfo,
    short: Option<String>,
}

impl DevShape<'_> {
    // parity: dev-classname.js — `{[devClass]: devClass, ...namespace}`
    fn dev_class(&self, namespace: &str) -> Option<String> {
        let prefix = self.dev_prefix.as_ref()?;
        Some(sanitize_dev_class_name(&format!("{prefix}{namespace}")))
    }

    // parity: add-sourcemap-data.js — namespaces without a known source line
    // (upstream cannot find them in the AST) keep `$$css: true`.
    fn css_marker(&mut self, namespace: &str) -> EvalValue {
        let Some(debug) = &mut self.debug else {
            return EvalValue::Bool(true);
        };
        match debug.lines.get(namespace) {
            Some(&line) if line > 0 => {
                let short = debug
                    .short
                    .get_or_insert_with(|| create_short_filename(debug.filename, &debug.info));
                if short.is_empty() {
                    EvalValue::Bool(true)
                } else {
                    EvalValue::Str(format!("{short}:{line}"))
                }
            }
            _ => EvalValue::Bool(true),
        }
    }
}

fn dev_prefix_for(ctx: &CreateContext<'_>, var_name: Option<&str>) -> Option<String> {
    (ctx.options.dev && ctx.options.enable_dev_class_names)
        .then(|| dev_class_prefix(var_name, ctx.filename.as_deref().unwrap_or("UnknownFile")))
}

fn compile_namespaces_core(
    namespaces: &JsObjectMap,
    ctx: &CreateContext<'_>,
    need_class_paths: bool,
    shape: &mut DevShape<'_>,
) -> Result<CreateOutput, StylexError> {
    let _t = crate::timings::start(crate::timings::Stage::Create);
    let options = ctx.options;
    let mut resolved_namespaces = JsObjectMap::with_capacity(namespaces.len());
    let mut rules: Vec<StylexRule> = Vec::new();
    let mut class_paths: Vec<(String, ClassPathsInNamespace)> = Vec::new();
    let mut seen_class_names: FxHashSet<Arc<str>> = FxHashSet::default();
    let mut decls: Vec<CompiledDecl> = Vec::new();

    for (namespace_name, namespace_value) in namespaces.entries() {
        // The evaluator's `value[key] =` [[Set]] never creates a "__proto__"
        // namespace, and Object.keys(namespaces) cannot see prototype entries.
        if namespace_name == "__proto__" {
            continue;
        }
        let EvalValue::Obj(namespace) = namespace_value else {
            return Err(StylexError::illegal_namespace_value());
        };

        let transformed;
        let entries = if namespace.get("__proto__").is_none()
            && namespace.entries().all(|(key, value)| {
                matches!(
                    value,
                    EvalValue::Null | EvalValue::Str(_) | EvalValue::Num(_)
                ) && !key.contains('(')
                    && is_unexpanded_property(key, options.style_resolution)
            }) {
            NamespaceEntries::Raw {
                namespace,
                index: 0,
            }
        } else {
            validate_namespace(namespace_value)?;
            let namespace_ref = match media_order_transform(namespace, options)? {
                Some(rebuilt) => {
                    transformed = rebuilt;
                    &transformed
                }
                None => namespace,
            };
            NamespaceEntries::Prepared(
                dedupe_last_wins(flatten_raw_style_object(namespace_ref, options)?).into_iter(),
            )
        };

        let mut namespace_obj = JsObjectMap::with_capacity(entries.len() + 2);
        if let Some(dev_class) = shape.dev_class(namespace_name) {
            namespace_obj.insert(dev_class.clone(), EvalValue::Str(dev_class));
        }
        let mut paths_in_namespace: Vec<(String, Vec<String>)> = Vec::new();
        for (key, pre_rule) in entries {
            // Variables-as-keys skip minification to avoid dynamic-style regressions.
            let display_key = if options.enable_minified_keys && !key.starts_with("--") {
                minified_display_key(&key, options.debug)
            } else {
                key.to_string()
            };

            decls.clear();
            pre_rule.for_each_compiled(options, &mut |decl, key_path| {
                if need_class_paths {
                    match paths_in_namespace
                        .iter_mut()
                        .find(|(name, _)| name.as_str() == &*decl.class_name)
                    {
                        Some(slot) => {
                            slot.1 = key_path.iter().map(|c| c.to_string()).collect();
                        }
                        None => paths_in_namespace.push((
                            decl.class_name.to_string(),
                            key_path.iter().map(|c| c.to_string()).collect(),
                        )),
                    }
                }
                decls.push(decl);
            })?;

            let mut joined =
                String::with_capacity(decls.iter().map(|d| d.class_name.len() + 1).sum());
            let mut first = true;
            for (i, decl) in decls.iter().enumerate() {
                if decls[..i]
                    .iter()
                    .any(|seen| seen.class_name == decl.class_name)
                {
                    continue;
                }
                if !first {
                    joined.push(' ');
                }
                first = false;
                joined.push_str(&decl.class_name);
            }
            namespace_obj.insert(
                display_key,
                if joined.is_empty() {
                    EvalValue::Null
                } else {
                    EvalValue::Str(joined)
                },
            );
            for decl in &decls {
                if seen_class_names.insert(decl.class_name.clone()) {
                    rules.push((**decl).clone());
                }
            }
        }
        namespace_obj.insert("$$css", shape.css_marker(namespace_name));
        resolved_namespaces.insert(
            namespace_name.to_string(),
            EvalValue::Obj(Arc::new(namespace_obj)),
        );
        if need_class_paths {
            class_paths.push((namespace_name.to_string(), paths_in_namespace));
        }
    }

    Ok(CreateOutput {
        compiled: Arc::new(resolved_namespaces),
        rules,
        class_paths,
    })
}

enum NamespaceEntries<'a> {
    Raw {
        namespace: &'a JsObjectMap,
        index: usize,
    },
    Prepared(std::vec::IntoIter<(Cow<'a, str>, PreRule<'a>)>),
}

impl<'a> Iterator for NamespaceEntries<'a> {
    type Item = (Cow<'a, str>, PreRule<'a>);

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len(), Some(self.len()))
    }
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Prepared(entries) => entries.next(),
            Self::Raw { namespace, index } => {
                if *index == namespace.len() {
                    return None;
                }
                let (key, value) = namespace.entry_at(*index);
                *index += 1;
                let key = Cow::Borrowed(key);
                let scalar = match value {
                    EvalValue::Null => return Some((key, PreRule::Null)),
                    EvalValue::Num(n) => StyleScalar::Num(*n),
                    EvalValue::Str(s) => StyleScalar::Str(Cow::Borrowed(s)),
                    _ => unreachable!("raw entries are scalar"),
                };
                Some((
                    key.clone(),
                    PreRule::Rule {
                        property: key,
                        value: PreRuleValue::Single(scalar),
                        key_path: None,
                    },
                ))
            }
        }
    }
}

impl ExactSizeIterator for NamespaceEntries<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Raw { namespace, index } => namespace.len() - index,
            Self::Prepared(entries) => entries.len(),
        }
    }
}

// parity: stylex-create.js reduceRight dedupe — the LAST occurrence of a key
// wins and keeps its (last) position.
fn dedupe_last_wins<'a>(
    flattened: Vec<(Cow<'a, str>, PreRule<'a>)>,
) -> Vec<(Cow<'a, str>, PreRule<'a>)> {
    let mut seen: FxHashSet<&str> =
        FxHashSet::with_capacity_and_hasher(flattened.len(), Default::default());
    if flattened.iter().all(|(key, _)| seen.insert(key)) {
        return flattened;
    }
    seen.clear();
    let mut keep = vec![false; flattened.len()];
    for (i, (key, _)) in flattened.iter().enumerate().rev() {
        if seen.insert(key) {
            keep[i] = true;
        }
    }
    flattened
        .into_iter()
        .zip(keep)
        .filter_map(|(entry, keep)| keep.then_some(entry))
        .collect()
}

thread_local! {
    // createShortHash('<'+'>'+key) is pure; flattened keys repeat across every
    // file in a design-system corpus, so `k{hash}` is computed once per process.
    static MINIFIED_KEYS: std::cell::RefCell<FxHashMap<String, String>> =
        std::cell::RefCell::new(FxHashMap::default());
}

fn minified_display_key(key: &str, debug: bool) -> String {
    let display = |k_hash: &str| {
        if debug {
            let mut out = String::with_capacity(key.len() + 1 + k_hash.len());
            out.push_str(key);
            out.push('-');
            out.push_str(k_hash);
            out
        } else {
            k_hash.to_string()
        }
    };
    MINIFIED_KEYS.with(|memo| {
        let mut memo = memo.borrow_mut();
        match memo.get(key) {
            Some(k_hash) => display(k_hash),
            None => {
                let hashed = create_short_hash(&format!("<>{key}"));
                let mut k_hash = String::with_capacity(hashed.len() + 1);
                k_hash.push('k');
                k_hash.push_str(&hashed);
                let out = display(&k_hash);
                memo.insert(key.to_string(), k_hash);
                out
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::StyleResolution;

    fn obj(entries: &[(&str, EvalValue)]) -> EvalValue {
        EvalValue::Obj(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<JsObjectMap>()
                .into(),
        )
    }

    fn s(v: &str) -> EvalValue {
        EvalValue::Str(v.to_string())
    }

    #[test]
    fn minimal_namespace_compiles_to_known_answers() {
        let namespaces: JsObjectMap = [(
            "a".to_string(),
            obj(&[("color", s("blue")), ("backgroundColor", s("red"))]),
        )]
        .into_iter()
        .collect();
        let options = ResolvedOptions::default();
        let ctx = CreateContext::new(&options);
        let out = compile_namespaces(&namespaces, &ctx, false).unwrap();
        assert_eq!(
            out.compiled.to_json(),
            serde_json::json!({
                "a": { "kMwMTN": "xju2f9n", "kWkggS": "xrkmrrc", "$$css": true }
            })
        );
        assert_eq!(out.rules.len(), 2);
        assert_eq!(&*out.rules[0].ltr, ".xju2f9n{color:blue}");
        assert_eq!(&*out.rules[1].ltr, ".xrkmrrc{background-color:red}");
    }

    #[test]
    fn flat_namespace_keeps_numeric_order_and_css_marker_position() {
        let mut namespaces = JsObjectMap::new();
        namespaces.insert(
            "a",
            obj(&[
                ("color", s("blue")),
                ("2", s("b")),
                ("1", s("a")),
                ("$$css", EvalValue::Null),
                ("width", EvalValue::Null),
            ]),
        );
        let options = ResolvedOptions {
            style_resolution: StyleResolution::PropertySpecificity,
            enable_minified_keys: false,
            ..ResolvedOptions::default()
        };
        let output = compile_namespaces(&namespaces, &CreateContext::new(&options), true).unwrap();
        let Some(EvalValue::Obj(namespace)) = output.compiled.get("a") else {
            panic!("compiled namespace");
        };
        assert_eq!(
            namespace.keys().collect::<Vec<_>>(),
            ["1", "2", "color", "$$css", "width"]
        );
        assert_eq!(namespace.get("$$css"), Some(&EvalValue::Bool(true)));
        assert_eq!(namespace.get("width"), Some(&EvalValue::Null));
        assert_eq!(
            output.class_paths[0]
                .1
                .iter()
                .map(|(_, path)| path.clone())
                .collect::<Vec<_>>(),
            vec![vec!["1"], vec!["2"], vec!["color"]]
        );
    }

    #[test]
    fn flattening_errors_precede_scalar_normalization_errors() {
        let mut namespaces = JsObjectMap::new();
        namespaces.insert("a", obj(&[("color", s("")), ("all", s("initial"))]));
        let options = ResolvedOptions {
            style_resolution: StyleResolution::PropertySpecificity,
            property_validation_mode: crate::options::PropertyValidationMode::Throw,
            ..ResolvedOptions::default()
        };
        let error =
            compile_namespaces(&namespaces, &CreateContext::new(&options), false).unwrap_err();
        assert_eq!(error, StylexError::banned_shorthand("all").unwrap());
    }

    #[test]
    fn non_object_namespace_errors() {
        let namespaces: JsObjectMap = [("a".to_string(), EvalValue::Null)].into_iter().collect();
        let options = ResolvedOptions::default();
        let ctx = CreateContext::new(&options);
        let err = compile_namespaces(&namespaces, &ctx, false).unwrap_err();
        assert_eq!(err.message, "A StyleX namespace must be an object.");
    }
}
