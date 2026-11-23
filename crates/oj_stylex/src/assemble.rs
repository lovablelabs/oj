// parity: stylex-0.19.2 packages/@stylexjs/babel-plugin/src/index.js processStylexRules

use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{BuildHasher, BuildHasherDefault};
use std::ops::ControlFlow;
use std::sync::{Arc, Weak};

use crate::fxhash::{FxHashMap, FxHashSet};
use crate::jsrt::is_js_whitespace;
use crate::rules::StylexRule;
use crate::shared::media_query::{WidthBound, WidthBoundKind, width_sort_bound};
use crate::shared::priorities::get_priority;
use crate::timings::{self, Stage};

const LOGICAL_FLOAT_START_VAR: &str = "--stylex-logical-start";
const LOGICAL_FLOAT_END_VAR: &str = "--stylex-logical-end";

pub type Comparator = fn(&str, &str) -> Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayersConfig {
    Off,
    On {
        before: Vec<String>,
        after: Vec<String>,
        prefix: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssembleConfig {
    pub use_layers: LayersConfig,
    pub use_legacy_classnames_sort: bool,
    pub legacy_disable_layers: bool,
    pub enable_ltr_rtl_comments: bool,
}

impl Default for AssembleConfig {
    fn default() -> Self {
        AssembleConfig {
            use_layers: LayersConfig::Off,
            use_legacy_classnames_sort: false,
            legacy_disable_layers: false,
            enable_ltr_rtl_comments: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AssembleError {
    // Message text mirrors the upstream throw byte for byte.
    #[error("circular reference detected for constant {0}")]
    CircularConst(String),
    #[error("unsupported processStylexRules config: {0}")]
    Unsupported(String),
    #[error("invalid processStylexRules config: {0}")]
    InvalidConfig(String),
}

impl AssembleConfig {
    // Mirrors the upstream raw-config parse: `boolean` is shorthand for `{useLayers}`,
    // `useLayers !== false` turns layers on, falsy prefix means no prefix.
    pub fn from_json(v: &serde_json::Value) -> Result<Self, AssembleError> {
        use serde_json::Value;
        let obj = match v {
            Value::Bool(b) => {
                return Ok(AssembleConfig {
                    use_layers: if *b {
                        LayersConfig::on_empty()
                    } else {
                        LayersConfig::Off
                    },
                    ..AssembleConfig::default()
                });
            }
            Value::Null => return Ok(AssembleConfig::default()),
            Value::Object(o) => o,
            other => {
                return Err(AssembleError::InvalidConfig(format!(
                    "expected boolean or object, got {other}"
                )));
            }
        };
        let use_layers = match obj.get("useLayers") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => LayersConfig::Off,
            Some(Value::Bool(true)) => LayersConfig::on_empty(),
            Some(Value::Object(l)) => {
                let list = |key: &str| -> Result<Vec<String>, AssembleError> {
                    match l.get(key) {
                        None | Some(Value::Null) => Ok(vec![]),
                        Some(Value::Array(a)) => a
                            .iter()
                            .map(|s| {
                                s.as_str().map(str::to_string).ok_or_else(|| {
                                    AssembleError::InvalidConfig(format!(
                                        "useLayers.{key} entries must be strings"
                                    ))
                                })
                            })
                            .collect(),
                        Some(other) => Err(AssembleError::InvalidConfig(format!(
                            "useLayers.{key} must be an array, got {other}"
                        ))),
                    }
                };
                let prefix = match l.get("prefix") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(other) => {
                        return Err(AssembleError::InvalidConfig(format!(
                            "useLayers.prefix must be a string, got {other}"
                        )));
                    }
                };
                LayersConfig::On {
                    before: list("before")?,
                    after: list("after")?,
                    prefix,
                }
            }
            Some(other) => {
                return Err(AssembleError::InvalidConfig(format!(
                    "useLayers must be boolean or object, got {other}"
                )));
            }
        };
        Ok(AssembleConfig {
            use_layers,
            use_legacy_classnames_sort: obj.get("useLegacyClassnamesSort").is_some_and(js_truthy),
            legacy_disable_layers: obj.get("legacyDisableLayers").is_some_and(js_truthy),
            enable_ltr_rtl_comments: obj.get("enableLTRRTLComments").is_some_and(js_truthy),
        })
    }
}

impl LayersConfig {
    fn on_empty() -> Self {
        LayersConfig::On {
            before: vec![],
            after: vec![],
            prefix: None,
        }
    }
}

fn js_truthy(v: &serde_json::Value) -> bool {
    use serde_json::Value;
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

// JS string|number const value; `Other` covers metadata that upstream would
// stringify via String().
#[derive(Clone, Debug)]
enum ConstVal {
    Str(String),
    Num(f64),
    Other(serde_json::Value),
}

// Equality asks whether a constant was redeclared with the value it had, so
// numbers compare by bits: a NaN constant equals itself instead of reading
// as changed on every edit.
impl PartialEq for ConstVal {
    fn eq(&self, other: &ConstVal) -> bool {
        match (self, other) {
            (ConstVal::Str(a), ConstVal::Str(b)) => a == b,
            (ConstVal::Num(a), ConstVal::Num(b)) => a.to_bits() == b.to_bits(),
            (ConstVal::Other(a), ConstVal::Other(b)) => a == b,
            _ => false,
        }
    }
}

impl ConstVal {
    fn from_json(v: &serde_json::Value) -> ConstVal {
        match v {
            serde_json::Value::String(s) => ConstVal::Str(s.clone()),
            serde_json::Value::Number(n) => ConstVal::Num(n.as_f64().unwrap_or(f64::NAN)),
            other => match crate::rules::non_finite_from_tag(other) {
                Some(x) => ConstVal::Num(x),
                None => ConstVal::Other(other.clone()),
            },
        }
    }

    fn to_js_string(&self) -> String {
        match self {
            ConstVal::Str(s) => s.clone(),
            ConstVal::Num(n) => crate::jsrt::js_number_to_string(*n),
            ConstVal::Other(v) => js_string_of_value(v),
        }
    }
}

fn js_string_of_value(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => crate::jsrt::js_number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => s.clone(),
        // Array.prototype.toString: elements joined by ',', null → ''.
        Value::Array(a) => a
            .iter()
            .map(|e| {
                if e.is_null() {
                    String::new()
                } else {
                    js_string_of_value(e)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

pub fn assemble(rules: &[StylexRule], cfg: &AssembleConfig) -> Result<String, AssembleError> {
    assemble_impl(rules.iter(), rules.len(), cfg, None)
}

pub fn assemble_with_comparator(
    rules: &[StylexRule],
    cfg: &AssembleConfig,
    cmp: Comparator,
) -> Result<String, AssembleError> {
    assemble_impl(rules.iter(), rules.len(), cfg, Some(cmp))
}

fn is_const_rule(r: &StylexRule) -> bool {
    r.const_key.is_some() && matches!(&r.const_val, Some(v) if !v.is_null())
}

// One constant as declared: its name (the rule key, referenced as
// `var(--name)`) and its value.
type ConstDecl = (Arc<str>, ConstVal);

fn const_decl(r: &StylexRule) -> ConstDecl {
    (
        r.class_name.clone(),
        ConstVal::from_json(r.const_val.as_ref().unwrap()),
    )
}

fn const_decls(rules: &[StylexRule]) -> impl Iterator<Item = ConstDecl> + '_ {
    rules.iter().filter(|r| is_const_rule(r)).map(const_decl)
}

type ConstSlots = FxHashMap<Arc<str>, u32>;

// Insertion-ordered name → value map; a repeated name keeps its slot, value
// last-wins (JS Map.set). Each value is then resolved in place, in order.
fn collect_resolved_consts(
    decls: impl Iterator<Item = ConstDecl>,
) -> Result<Vec<ConstDecl>, AssembleError> {
    let mut consts: Vec<ConstDecl> = Vec::new();
    let mut slots = ConstSlots::default();
    for (name, val) in decls {
        match slots.entry(name) {
            Entry::Occupied(slot) => consts[*slot.get() as usize].1 = val,
            Entry::Vacant(slot) => {
                consts.push((slot.key().clone(), val));
                slot.insert(consts.len() as u32 - 1);
            }
        }
    }
    for i in 0..consts.len() {
        if let Some(resolved) = resolve_constant(&consts[i].1, &consts, &slots, &mut Vec::new())? {
            consts[i].1 = ConstVal::Str(resolved);
        }
    }
    Ok(consts)
}

// Read from the const-substituted text, as upstream's getLogicalFloatVars is.
fn text_has_logical_float(ltr: &str, rtl: Option<&str>) -> bool {
    fn has(text: &str) -> bool {
        text.contains("--stylex-logical-")
            && (text.contains(LOGICAL_FLOAT_START_VAR) || text.contains(LOGICAL_FLOAT_END_VAR))
    }
    has(ltr) || rtl.is_some_and(has)
}

fn logical_float_block(has_logical_float: bool) -> String {
    if has_logical_float {
        format!(
            ":root, [dir=\"ltr\"] {{\n  {LOGICAL_FLOAT_START_VAR}: left;\n  \
             {LOGICAL_FLOAT_END_VAR}: right;\n}}\n[dir=\"rtl\"] {{\n  \
             {LOGICAL_FLOAT_START_VAR}: right;\n  {LOGICAL_FLOAT_END_VAR}: left;\n}}\n"
        )
    } else {
        String::new()
    }
}

fn layer_parts(cfg: &AssembleConfig) -> (bool, &[String], &[String], &str) {
    match &cfg.use_layers {
        LayersConfig::On {
            before,
            after,
            prefix,
        } => (true, before, after, prefix.as_deref().unwrap_or("")),
        LayersConfig::Off => (false, &[], &[], ""),
    }
}

fn layer_name(prefix: &str, index: usize) -> String {
    if prefix.is_empty() {
        format!("priority{}", index + 1)
    } else {
        format!("{prefix}.priority{}", index + 1)
    }
}

fn layers_header(cfg: &AssembleConfig, group_count: usize) -> String {
    let (_, before, after, prefix) = layer_parts(cfg);
    let names: Vec<String> = before
        .iter()
        .cloned()
        .chain((0..group_count).map(|i| layer_name(prefix, i)))
        .chain(after.iter().cloned())
        .collect();
    format!("\n@layer {};\n", names.join(", "))
}

// One deduped class's output: the ltr line, or the ltr/rtl pair joined by the
// same '\n' the group join would use. The common shape (no rtl, no theme
// pair) is pushed as slices; `", ."` is a literal every theme-pair match
// contains.
fn push_chunk(out: &mut String, ltr: &str, rtl: Option<&str>, index: usize, cfg: &AssembleConfig) {
    let use_layers_on = matches!(cfg.use_layers, LayersConfig::On { .. });
    let levels = if use_layers_on || cfg.legacy_disable_layers {
        0
    } else {
        index
    };
    match rtl {
        None => push_rule_text(out, ltr, levels),
        Some(rtl) if cfg.enable_ltr_rtl_comments => {
            out.push_str("/* @ltr begin */");
            push_rule_text(out, ltr, levels);
            out.push_str("/* @ltr end */\n/* @rtl begin */");
            push_rule_text(out, rtl, levels);
            out.push_str("/* @rtl end */");
        }
        Some(rtl) => {
            push_with_ancestor(out, ltr, levels, "html:not([dir='rtl']) ");
            out.push('\n');
            push_with_ancestor(out, rtl, levels, "html[dir='rtl'] ");
        }
    }
}

// One direction's text as upstream prints it: the specificity bump, then the
// theme-class doubling over the bumped text.
fn push_rule_text(out: &mut String, text: &str, levels: usize) {
    let start = out.len();
    if levels == 0 {
        out.push_str(text);
    } else {
        push_specificity_level(out, text, levels);
    }
    if has_theme_pair_hint(&out[start..]) {
        let doubled = double_theme_classes(&out[start..]);
        out.truncate(start);
        out.push_str(&doubled);
    }
}

// parity: addAncestorSelector over the printed text. A plain selector takes
// the ancestor in front; an at-rule takes it after the `{` that follows its
// last `@` (an indexOf miss there puts it in front, as slice(0, 0) does).
// `ancestor` carries its trailing space.
fn push_with_ancestor(out: &mut String, text: &str, levels: usize, ancestor: &str) {
    let start = out.len();
    push_rule_text(out, text, levels);
    let printed = &out[start..];
    if is_selector_exempt(printed) {
        return;
    }
    let at = if printed.starts_with('@') {
        let last_at_rule = printed.rfind('@').unwrap_or(0);
        printed[last_at_rule..]
            .find('{')
            .map_or(0, |brace| last_at_rule + brace + 1)
    } else {
        0
    };
    out.insert_str(start + at, ancestor);
}

// One non-constant rule as upstream sorts and prints it: constants substituted
// and the priority of a constant at-rule wrapper re-priced, both before the sort.
struct Prepped<'a> {
    rule: &'a StylexRule,
    ltr: Option<String>,
    rtl: Option<String>,
    priority: f64,
    width: Option<Arc<WidthSortKey>>,
}

impl<'a> Prepped<'a> {
    fn new(rule: &'a StylexRule, legacy: bool, consts: &ConstTable) -> Self {
        let ltr = substitute_consts(&rule.ltr, consts);
        let rtl = rule
            .rtl
            .as_deref()
            .and_then(|text| substitute_consts(text, consts));
        // The legacy class-name sort never reads the width key.
        let width = if legacy {
            None
        } else {
            width_sort_key(ltr.as_deref().unwrap_or(&rule.ltr))
        };
        Prepped {
            rule,
            priority: adjust_const_at_rule_priority(&rule.ltr, rule.priority, consts),
            ltr,
            rtl,
            width,
        }
    }

    fn ltr(&self) -> &str {
        self.ltr.as_deref().unwrap_or(&self.rule.ltr)
    }

    fn rtl(&self) -> Option<&str> {
        self.rtl.as_deref().or(self.rule.rtl.as_deref())
    }
}

fn compare_priorities(a: f64, b: f64) -> Option<Ordering> {
    let diff = a - b;
    if diff == 0.0 {
        None
    } else if diff < 0.0 {
        Some(Ordering::Less)
    } else {
        Some(Ordering::Greater)
    }
}

fn compare_rules(
    a: &Prepped<'_>,
    b: &Prepped<'_>,
    cfg: &AssembleConfig,
    cmp: Comparator,
) -> Ordering {
    if let Some(by_priority) = compare_priorities(a.priority, b.priority) {
        return by_priority;
    }
    if cfg.use_legacy_classnames_sort {
        return cmp(&a.rule.class_name, &b.rule.class_name);
    }
    let (rule_a, rule_b) = (a.ltr(), b.ltr());
    cmp(
        declared_property_name(rule_a),
        declared_property_name(rule_b),
    )
    .then_with(|| compare_width_sort_keys(a.width.as_deref(), b.width.as_deref(), cmp))
    .then_with(|| cmp(decl_slice(rule_a), decl_slice(rule_b)))
    .then_with(|| cmp(rule_a, rule_b))
}

// `rule_count` is how many rules the iterator yields; a flattened iterator's
// own size hint is no use for sizing the list up front.
fn assemble_impl<'a>(
    rules: impl Iterator<Item = &'a StylexRule>,
    rule_count: usize,
    cfg: &AssembleConfig,
    custom_cmp: Option<Comparator>,
) -> Result<String, AssembleError> {
    let consts_timer = timings::start(Stage::AssembleConsts);
    let mut decls: Vec<ConstDecl> = Vec::new();
    let mut non_const: Vec<&StylexRule> = Vec::with_capacity(rule_count);
    let mut any_rule = false;
    for rule in rules {
        any_rule = true;
        if is_const_rule(rule) {
            decls.push(const_decl(rule));
        } else {
            non_const.push(rule);
        }
    }
    if !any_rule {
        return Ok(String::new());
    }
    let consts = prepare_consts(&collect_resolved_consts(decls.into_iter())?);
    drop(consts_timer);

    let legacy = cfg.use_legacy_classnames_sort;
    // `order` lists `items` positions in print order; a position repeats when
    // one prepared rule stands in for its duplicates.
    let (items, order): (Vec<Prepped<'_>>, Vec<u32>) = match custom_cmp {
        Some(cmp) => {
            let subst_timer = timings::start(Stage::AssembleSubst);
            let mut items: Vec<Prepped<'_>> = non_const
                .iter()
                .map(|rule| Prepped::new(rule, legacy, &consts))
                .collect();
            drop(subst_timer);
            let _t = timings::start(Stage::AssembleSort);
            items.sort_by(|a, b| compare_rules(a, b, cfg, cmp));
            let order = (0..items.len() as u32).collect();
            (items, order)
        }
        None => keyed_order(&non_const, legacy, &consts),
    };
    let _render_timer = timings::start(Stage::AssembleRender);
    // The print order as one flat run: the passes below read it in sequence
    // instead of chasing each rule through its prepared copy.
    let sorted: Vec<Printed<'_>> = order
        .iter()
        .map(|&i| {
            let item = &items[i as usize];
            Printed {
                priority: item.priority,
                class_name: &item.rule.class_name,
                ltr: item.ltr(),
                rtl: item.rtl(),
            }
        })
        .collect();

    let logical_float_vars = logical_float_block(
        sorted
            .iter()
            .any(|item| text_has_logical_float(item.ltr, item.rtl)),
    );

    // Consecutive runs of floor(priority/1000); input is priority-sorted so runs
    // are the priority bands.
    let mut bands: Vec<(usize, usize)> = Vec::new();
    let mut last_level = -1.0f64;
    for (i, item) in sorted.iter().enumerate() {
        let level = (item.priority / 1000.0).floor();
        match bands.last_mut() {
            Some(band) if level == last_level => band.1 = i + 1,
            _ => {
                last_level = level;
                bands.push((i, i + 1));
            }
        }
    }

    let (use_layers_on, _, _, layer_prefix) = layer_parts(cfg);
    let header = if use_layers_on {
        layers_header(cfg, bands.len())
    } else {
        String::new()
    };

    let text_bytes: usize = sorted
        .iter()
        .map(|item| item.ltr.len() + item.rtl.map_or(0, |t| t.len() + 40) + 1)
        .sum();
    let mut out = String::with_capacity(
        logical_float_vars.len() + header.len() + text_bytes + 32 * bands.len(),
    );
    out.push_str(&logical_float_vars);
    out.push_str(&header);

    // With layers on, a band prints its priority-0 rules bare and the rest
    // inside the band's @layer; each part dedupes on its own.
    let parts: &[BandPart] = if use_layers_on {
        &[BandPart::Unlayered, BandPart::Layered]
    } else {
        &[BandPart::Whole]
    };
    let mut slots: Vec<usize> = Vec::new();
    let mut slot_of: FxHashMap<&str, usize> = FxHashMap::default();
    for (index, &(start, end)) in bands.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let mut printed_part = false;
        for &part in parts {
            // Last-wins dedupe by className, first-occurrence order (JS Map semantics).
            slots.clear();
            slot_of.clear();
            for (offset, item) in sorted[start..end].iter().enumerate() {
                if BandPart::of(use_layers_on, item.priority) != Some(part) {
                    continue;
                }
                let i = start + offset;
                match slot_of.entry(item.class_name) {
                    Entry::Vacant(vacant) => {
                        vacant.insert(slots.len());
                        slots.push(i);
                    }
                    Entry::Occupied(occupied) => slots[*occupied.get()] = i,
                }
            }
            if part != BandPart::Whole && slots.is_empty() {
                continue;
            }
            if printed_part {
                out.push('\n');
            }
            printed_part = true;
            if part == BandPart::Layered {
                out.push_str("@layer ");
                out.push_str(&layer_name(layer_prefix, index));
                out.push_str("{\n");
            }
            for (j, &i) in slots.iter().enumerate() {
                if j > 0 {
                    out.push('\n');
                }
                let item = &sorted[i];
                // Empty rtl is falsy upstream and behaves exactly like absent.
                let rtl = item.rtl.filter(|s| !s.is_empty());
                push_chunk(&mut out, item.ltr, rtl, index, cfg);
            }
            if part == BandPart::Layered {
                out.push_str("\n}");
            }
        }
    }
    Ok(out)
}

// What printing reads of one rule, in print order.
struct Printed<'a> {
    priority: f64,
    class_name: &'a str,
    ltr: &'a str,
    rtl: Option<&'a str>,
}

// How one priority band is cut for printing. Upstream filters `pri === 0` and
// `pri > 0`, so under layers a rule that is neither is not printed at all.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BandPart {
    Whole,
    Unlayered,
    Layered,
}

impl BandPart {
    // The part a rule of this priority prints in; None = not printed.
    fn of(use_layers_on: bool, priority: f64) -> Option<BandPart> {
        if !use_layers_on {
            Some(BandPart::Whole)
        } else if priority > 0.0 {
            Some(BandPart::Layered)
        } else if priority == 0.0 {
            Some(BandPart::Unlayered)
        } else {
            None
        }
    }
}

// rule.slice(rule.lastIndexOf('{') + 1) up to its first ':' (a miss keeps the
// whole declaration; a lastIndexOf miss keeps the whole rule).
fn declared_property_name(rule: &str) -> &str {
    let (start, len) = declared_name_range(rule);
    &rule[start..start + len]
}

fn declared_name_range(rule: &str) -> (usize, usize) {
    let start = rule.rfind('{').map_or(0, |i| i + 1);
    let len = rule[start..].find(':').unwrap_or(rule.len() - start);
    (start, len)
}

// The single px width bound of a rule's one `@media` wrapper, with the wrapper
// chain it sits in; same-property rules order by it before their text.
#[derive(Debug, PartialEq)]
struct WidthSortKey {
    context: Box<str>,
    bound: WidthBound,
}

thread_local! {
    // The key is a pure function of the at-rule prefix, and a stylesheet
    // repeats the same few wrappers on most of its conditional rules.
    static WIDTH_KEY_MEMO: std::cell::RefCell<FxHashMap<Box<str>, Option<Arc<WidthSortKey>>>> =
        std::cell::RefCell::new(FxHashMap::default());
}

const WIDTH_KEY_MEMO_CAP: usize = 1 << 14;

// parity: getWidthSortKey. The at-rule prefix is every leading `@…{` wrapper,
// which is what atRulePreludes walks.
fn width_sort_key(rule: &str) -> Option<Arc<WidthSortKey>> {
    let bytes = rule.as_bytes();
    let mut prefix_len = 0;
    let mut has_media = false;
    while bytes.get(prefix_len) == Some(&b'@') {
        match memchr::memchr(b'{', &bytes[prefix_len..]) {
            Some(brace) => {
                has_media |= bytes[prefix_len..].starts_with(b"@media ");
                prefix_len += brace + 1;
            }
            None => break,
        }
    }
    // A chain has a key only through a `@media ` prelude; every other rule
    // (no wrapper, @keyframes, @supports, …) is answered without the memo.
    if !has_media {
        return None;
    }
    let prefix = &rule[..prefix_len];
    if let Some(hit) = WIDTH_KEY_MEMO.with(|memo| memo.borrow().get(prefix).cloned()) {
        return hit;
    }
    let key = width_sort_key_for_chain(prefix).map(Arc::new);
    WIDTH_KEY_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if memo.len() >= WIDTH_KEY_MEMO_CAP {
            memo.clear();
        }
        memo.insert(prefix.into(), key.clone());
    });
    key
}

// parity: widthSortKeyForChain over atRulePreludes.
fn width_sort_key_for_chain(prefix: &str) -> Option<WidthSortKey> {
    let preludes: Vec<&str> = prefix
        .split_terminator('{')
        .map(|prelude| prelude.trim_end_matches(is_js_whitespace))
        .collect();
    let mut media = preludes
        .iter()
        .enumerate()
        .filter(|(_, prelude)| prelude.starts_with("@media "));
    let (media_index, prelude) = media.next()?;
    if media.next().is_some() {
        return None;
    }
    let bound = width_sort_bound(prelude)?;
    let context: Vec<&str> = preludes
        .iter()
        .enumerate()
        .map(|(i, prelude)| if i == media_index { "@media" } else { prelude })
        .collect();
    Some(WidthSortKey {
        context: context.join("{").into(),
        bound,
    })
}

// parity: compareWidthSortKeys — keyless rules last, then by wrapper chain,
// min-width ascending before max-width descending.
fn compare_width_sort_keys(
    a: Option<&WidthSortKey>,
    b: Option<&WidthSortKey>,
    cmp: Comparator,
) -> Ordering {
    let (a, b) = match (a, b) {
        (None, None) => return Ordering::Equal,
        (None, Some(_)) => return Ordering::Greater,
        (Some(_), None) => return Ordering::Less,
        (Some(a), Some(b)) => (a, b),
    };
    if a.context != b.context {
        return cmp(&a.context, &b.context);
    }
    let by_value = |x: f64, y: f64| x.partial_cmp(&y).unwrap_or(Ordering::Equal);
    match (a.bound.kind, b.bound.kind) {
        (WidthBoundKind::Min, WidthBoundKind::Min) => by_value(a.bound.value, b.bound.value),
        (WidthBoundKind::Max, WidthBoundKind::Max) => by_value(b.bound.value, a.bound.value),
        (WidthBoundKind::Min, WidthBoundKind::Max) => Ordering::Less,
        (WidthBoundKind::Max, WidthBoundKind::Min) => Ordering::Greater,
    }
}

// parity: adjustConstAtRulePriority. A rule wrapped in a constant placeholder
// (`var(--hash){…}`) was priced as an unknown key; once the constant is known
// to be an at-rule, that share is swapped for the at-rule's own.
fn adjust_const_at_rule_priority(ltr: &str, priority: f64, consts: &ConstTable) -> f64 {
    if !consts.has_at_rules {
        return priority;
    }
    let mut adjusted = priority;
    let mut from = 0;
    while let Some((start, end)) = find_var_ref(ltr, from) {
        from = end;
        if ltr.as_bytes().get(end) != Some(&b'{') {
            continue;
        }
        // The placeholder is a scanner match, so only a `by_name` constant can
        // be the one it names.
        let shares = consts
            .by_name
            .get(&ltr[start + 6..end - 1])
            .and_then(|&i| consts.consts[i as usize].at_rule_shares);
        if let Some((placeholder_share, at_rule_share)) = shares
            && adjusted >= placeholder_share
        {
            adjusted += at_rule_share - placeholder_share;
        }
    }
    adjusted
}

// The UTF-16 code units of the class name (legacy) or of the const-substituted
// rule text, derived once. The declaration and the declared property name are
// subranges of the same units, so every string tie-break uses the one order.
// Offsets are u32 to keep the key, and the prepared rule around it, small.
struct RuleKey {
    units: Box<[u16]>,
    declaration_start: u32,
    name: (u32, u32),
}

fn key_offset(units: usize) -> u32 {
    u32::try_from(units).expect("a rule's text stays under 4 GiB")
}

// An ASCII text is its own code units, at the same offsets.
fn utf16_units(text: &str) -> Box<[u16]> {
    if text.is_ascii() {
        text.bytes().map(u16::from).collect()
    } else {
        text.encode_utf16().collect()
    }
}

impl RuleKey {
    // `ltr` is the const-substituted rule text, the one upstream sorts on.
    fn for_text(class_name: &str, ltr: &str, legacy: bool) -> Self {
        if legacy {
            let units = utf16_units(class_name);
            return Self {
                declaration_start: key_offset(units.len()),
                units,
                name: (0, 0),
            };
        }
        let units = utf16_units(ltr);
        let ascii = units.len() == ltr.len();
        let at = |bytes: usize| {
            let units = if ascii {
                bytes
            } else {
                ltr[..bytes].encode_utf16().count()
            };
            key_offset(units)
        };
        let (name_start, name_len) = declared_name_range(ltr);
        Self {
            declaration_start: at(ltr.len() - decl_slice(ltr).len()),
            name: (at(name_start), at(name_start + name_len)),
            units,
        }
    }

    fn name(&self) -> &[u16] {
        &self.units[self.name.0 as usize..self.name.1 as usize]
    }

    fn declaration(&self) -> &[u16] {
        &self.units[self.declaration_start as usize..]
    }

    // The order of the steps is upstream's: declared name, width bound,
    // declaration, whole rule (the legacy class-name key has neither a name
    // nor a declaration).
    fn cmp(
        &self,
        other: &Self,
        width: Option<&WidthSortKey>,
        other_width: Option<&WidthSortKey>,
    ) -> Ordering {
        self.name()
            .cmp(other.name())
            .then_with(|| self.cmp_after_name(other, width, other_width))
    }

    // The steps of `cmp` that follow an equal declared name.
    fn cmp_after_name(
        &self,
        other: &Self,
        width: Option<&WidthSortKey>,
        other_width: Option<&WidthSortKey>,
    ) -> Ordering {
        compare_width_sort_keys(width, other_width, default_locale_cmp)
            .then_with(|| self.declaration().cmp(other.declaration()))
            .then_with(|| self.units.cmp(&other.units))
    }
}

// A finite priority as bits that order like the number does; -0 and +0 are
// one priority, as their difference is 0 upstream.
fn priority_bits(priority: f64) -> u64 {
    let bits = (priority + 0.0).to_bits();
    if bits >> 63 == 0 {
        bits | (1 << 63)
    } else {
        !bits
    }
}

// One rule in `sort_finite`: its priority and the rank of its declared
// property name as integers, so most comparisons never read the key units.
struct SortSlot<T> {
    priority: u64,
    name: u32,
    id: T,
}

// The print order of rules whose priorities are all finite. That comparator
// is a total preorder, so the stable sort's answer is the one order sorted by
// (comparator, input position): `ids` arrive in any order and `T` orders
// them by input position.
fn sort_finite<'k, T: Copy + Ord>(
    ids: impl Iterator<Item = T>,
    rule: impl Fn(T) -> (f64, &'k RuleKey, Option<&'k WidthSortKey>),
) -> Vec<T> {
    let mut name_ids: FxHashMap<&'k [u16], u32> = FxHashMap::default();
    let mut names: Vec<&'k [u16]> = Vec::new();
    let mut slots: Vec<SortSlot<T>> = ids
        .map(|id| {
            let (priority, key, _) = rule(id);
            let name = key.name();
            let name = *name_ids.entry(name).or_insert_with(|| {
                names.push(name);
                names.len() as u32 - 1
            });
            SortSlot {
                priority: priority_bits(priority),
                name,
                id,
            }
        })
        .collect();
    // Code-unit slices compare as utf16_cmp compares the names they spell.
    let mut by_rank: Vec<u32> = (0..names.len() as u32).collect();
    by_rank.sort_unstable_by(|&a, &b| names[a as usize].cmp(names[b as usize]));
    let mut rank = vec![0u32; names.len()];
    for (position, &name) in by_rank.iter().enumerate() {
        rank[name as usize] = position as u32;
    }
    for slot in &mut slots {
        slot.name = rank[slot.name as usize];
    }
    slots.sort_unstable_by(|a, b| {
        (a.priority, a.name)
            .cmp(&(b.priority, b.name))
            .then_with(|| {
                let (_, key, width) = rule(a.id);
                let (_, other_key, other_width) = rule(b.id);
                key.cmp_after_name(other_key, width, other_width)
            })
            .then_with(|| a.id.cmp(&b.id))
    });
    slots.into_iter().map(|slot| slot.id).collect()
}

// Keys derived once per distinct rule. Returns the prepared rules and their
// print order as positions into them.
fn keyed_order<'a>(
    non_const: &[&'a StylexRule],
    legacy: bool,
    consts: &ConstTable,
) -> (Vec<Prepped<'a>>, Vec<u32>) {
    let dedupe_timer = timings::start(Stage::AssembleDedupe);
    let deduped = dedupe_keep_first(non_const);
    drop(dedupe_timer);
    let unique: &[&'a StylexRule] = deduped.as_ref().map_or(non_const, |d| &d.unique);

    let subst_timer = timings::start(Stage::AssembleSubst);
    let items: Vec<Prepped<'a>> = unique
        .iter()
        .map(|rule| Prepped::new(rule, legacy, consts))
        .collect();
    drop(subst_timer);
    let keys_timer = timings::start(Stage::AssembleKeys);
    let keys: Vec<RuleKey> = items
        .iter()
        .map(|item| RuleKey::for_text(&item.rule.class_name, item.ltr(), legacy))
        .collect();
    drop(keys_timer);

    let _sort_timer = timings::start(Stage::AssembleSort);
    if items.iter().all(|item| item.priority.is_finite()) {
        // Copies sort Equal and adjacent, so the dropped ones change nothing.
        let order = sort_finite(0..items.len() as u32, |i| {
            let item = &items[i as usize];
            (item.priority, &keys[i as usize], item.width.as_deref())
        });
        return (items, order);
    }
    // Without a consistent total order (a non-finite priority) dropping
    // duplicates is not order-neutral: the full input is sorted, every rule
    // through the copy prepared for it.
    let mut order: Vec<u32> = match deduped {
        Some(deduped) => deduped.stand_in,
        None => (0..items.len() as u32).collect(),
    };
    order.sort_by(|&ia, &ib| {
        let (a, b) = (&items[ia as usize], &items[ib as usize]);
        compare_priorities(a.priority, b.priority).unwrap_or_else(|| {
            keys[ia as usize].cmp(&keys[ib as usize], a.width.as_deref(), b.width.as_deref())
        })
    });
    (items, order)
}

struct Deduped<'a> {
    unique: Vec<&'a StylexRule>,
    // Per input rule, the position in `unique` of the copy that stands for it.
    stand_in: Vec<u32>,
}

// Keep-first is output-neutral (copies sort Equal, adjacent); a class with two
// distinct texts keeps every copy, since a copy between them decides last-wins.
// None = the input holds no copies.
fn dedupe_keep_first<'a>(rules: &[&'a StylexRule]) -> Option<Deduped<'a>> {
    let mut seen: FxHashMap<(&str, &str, Option<&str>, u64), u32> =
        FxHashMap::with_capacity_and_hasher(rules.len(), Default::default());
    // Per input rule, the input position of its first copy.
    let first: Vec<u32> = rules
        .iter()
        .enumerate()
        .map(|(i, r)| {
            *seen
                .entry((
                    &r.class_name,
                    &r.ltr,
                    r.rtl.as_deref(),
                    r.priority.to_bits(),
                ))
                .or_insert(i as u32)
        })
        .collect();
    if seen.len() == rules.len() {
        return None;
    }
    let is_first = |i: usize| first[i] as usize == i;
    let mut texts: FxHashMap<&str, u32> = FxHashMap::default();
    for (i, r) in rules.iter().enumerate() {
        if is_first(i) {
            *texts.entry(&r.class_name).or_default() += 1;
        }
    }
    let single_text_classes = texts.len() == seen.len();
    let mut unique = Vec::with_capacity(seen.len());
    let mut stand_in: Vec<u32> = Vec::with_capacity(rules.len());
    for (i, r) in rules.iter().enumerate() {
        if is_first(i) || (!single_text_classes && texts[&*r.class_name] > 1) {
            stand_in.push(unique.len() as u32);
            unique.push(*r);
        } else {
            stand_in.push(stand_in[first[i] as usize]);
        }
    }
    Some(Deduped { unique, stand_in })
}

// rule.slice(rule.lastIndexOf('{')): lastIndexOf misses → -1 → JS slice(-1) is
// the final character.
fn decl_slice(rule: &str) -> &str {
    match rule.rfind('{') {
        Some(i) => &rule[i..],
        None => rule
            .char_indices()
            .last()
            .map(|(i, _)| &rule[i..])
            .unwrap_or(""),
    }
}

// parity: resolveConstant. `None` = the value references no constant and
// stays as it is; `visiting` holds the slots of the references being expanded.
fn resolve_constant(
    value: &ConstVal,
    consts: &[ConstDecl],
    slots: &ConstSlots,
    visiting: &mut Vec<u32>,
) -> Result<Option<String>, AssembleError> {
    let ConstVal::Str(text) = value else {
        return Ok(None);
    };
    let mut result: Option<String> = None;
    let mut scan_from = 0usize;
    loop {
        let cur = result.as_deref().unwrap_or(text);
        let Some((start, end)) = find_var_ref(cur, scan_from) else {
            break;
        };
        // Only a constant can be under expansion, so the cycle check upstream
        // runs before its lookup reads the same after it.
        let Some(&slot) = slots.get(&cur[start + 6..end - 1]) else {
            scan_from = end;
            continue;
        };
        if visiting.contains(&slot) {
            return Err(AssembleError::CircularConst(
                cur[start + 4..end - 1].to_string(),
            ));
        }
        visiting.push(slot);
        let ref_val = &consts[slot as usize].1;
        let replacement = match resolve_constant(ref_val, consts, slots, visiting)? {
            Some(resolved) => resolved,
            None => ref_val.to_js_string(),
        };
        visiting.pop();
        // The replacement is inserted verbatim (a function replacer upstream):
        // `$&` and friends in a constant's value are plain text. Upstream
        // replaces the first occurrence of the matched text, which is this
        // match: an earlier one would have been matched, and replaced, first.
        let mut next = String::with_capacity(cur.len() - (end - start) + replacement.len());
        next.push_str(&cur[..start]);
        next.push_str(&replacement);
        next.push_str(&cur[end..]);
        result = Some(next);
        scan_from = 0;
    }
    Ok(result)
}

// Scanner for /var\((--[A-Za-z0-9_-]+)\)/ from a byte offset; returns the byte
// span of the whole match. `(` is the rarest byte of `var(--`, so it anchors.
fn find_var_ref(s: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = s.as_bytes();
    let mut pos = from;
    loop {
        let paren = pos + memchr::memchr(b'(', bytes.get(pos..)?)?;
        pos = paren + 1;
        if paren < from + 3
            || &bytes[paren - 3..paren] != b"var"
            || bytes.get(paren + 1..paren + 3) != Some(b"--")
        {
            continue;
        }
        let mut j = paren + 3;
        while j < bytes.len() && is_var_ident_byte(bytes[j]) {
            j += 1;
        }
        if j > paren + 3 && j < bytes.len() && bytes[j] == b')' {
            return Some((paren - 3, j + 1));
        }
    }
}

fn is_var_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

// Every `--name:` in `text` whose name is one or more identifier bytes, i.e.
// every place a regular constant's key-rewrite needle can match. The visitor
// ends the walk by breaking.
fn for_each_custom_property_key<'a>(
    text: &'a str,
    mut visit: impl FnMut(&'a str) -> ControlFlow<()>,
) -> ControlFlow<()> {
    let bytes = text.as_bytes();
    for colon in memchr::memchr_iter(b':', bytes) {
        let mut run = colon;
        while run > 0 && is_var_ident_byte(bytes[run - 1]) {
            run -= 1;
        }
        for at in run..colon.saturating_sub(2) {
            if bytes[at] == b'-' && bytes[at + 1] == b'-' {
                visit(&text[at + 2..colon])?;
            }
        }
    }
    ControlFlow::Continue(())
}

// The per-const strings substitution needs, derived once per assemble instead
// of once per rule × const.
struct PreparedConst {
    var_ref: String,
    replacement: String,
    key_rewrite: Option<(String, String)>,
    // For a string constant that is an at-rule: the priority share its
    // placeholder was given as a key, and the share the at-rule itself earns.
    at_rule_shares: Option<(f64, f64)>,
}

impl PreparedConst {
    // Whether this constant's substitution step would rewrite `text`.
    fn touches(&self, text: &str) -> bool {
        text.contains(self.var_ref.as_str())
            || self
                .key_rewrite
                .as_ref()
                .is_some_and(|(needle, _)| text.contains(needle.as_str()))
    }

    // parity: one iteration of the per-rule constsMap loop, over the running
    // text (`out`, or `text` while nothing has matched yet).
    fn substitute(&self, text: &str, out: &mut Option<String>) {
        let cur = out.as_deref().unwrap_or(text);
        if cur.contains(self.var_ref.as_str()) {
            // Verbatim, like the function replacer upstream passes; only the
            // key rewrite below still reads `$` patterns in its replacement.
            *out = Some(cur.replace(self.var_ref.as_str(), &self.replacement));
        }
        let cur = out.as_deref().unwrap_or(text);
        if let Some((needle, target)) = &self.key_rewrite
            && cur.contains(needle.as_str())
        {
            *out = Some(js_replace_all(cur, needle, target));
        }
    }
}

// The resolved constants in upstream's iteration order, indexed so a rule is
// scanned once for the constants it mentions instead of once per constant.
#[derive(Default)]
struct ConstTable {
    consts: Vec<PreparedConst>,
    // Constants whose `var(--name)` the reference scanner reads (an
    // identifier name): name → position in `consts`.
    by_name: FxHashMap<Arc<str>, u32>,
    // The rest, ascending; each is probed by substring search like upstream.
    irregular: Vec<u32>,
    // Any `by_name` constant rewrites declaration keys / is an at-rule.
    rewrites_keys: bool,
    has_at_rules: bool,
}

fn prepare_consts(consts: &[ConstDecl]) -> ConstTable {
    let mut table = ConstTable::default();
    for (i, (name, const_val)) in consts.iter().enumerate() {
        let var_ref = format!("var(--{name})");
        let replacement = const_val.to_js_string();
        // A const resolving to var(...) also rewrites `--constName:` declaration
        // keys so the target variable stays overridable. Trims are ES TrimString.
        let key_rewrite =
            (replacement.starts_with("var(") && replacement.ends_with(')')).then(|| {
                let inside = crate::jsrt::js_trim(&replacement[4..replacement.len() - 1]);
                let target_name = match inside.find(',') {
                    Some(ci) => crate::jsrt::js_trim(&inside[..ci]),
                    None => inside,
                };
                (format!("--{name}:"), format!("{target_name}:"))
            });
        let at_rule_shares = match const_val {
            ConstVal::Str(value) if value.starts_with('@') => {
                Some((get_priority(&var_ref), get_priority(value)))
            }
            _ => None,
        };
        if !name.is_empty() && name.bytes().all(is_var_ident_byte) {
            table.by_name.insert(name.clone(), i as u32);
            table.rewrites_keys |= key_rewrite.is_some();
            table.has_at_rules |= at_rule_shares.is_some();
        } else {
            table.irregular.push(i as u32);
        }
        table.consts.push(PreparedConst {
            var_ref,
            replacement,
            key_rewrite,
            at_rule_shares,
        });
    }
    table
}

impl ConstTable {
    fn is_empty(&self) -> bool {
        self.consts.is_empty()
    }

    // The first constant at or after `from` whose substitution step would
    // rewrite `text`: the steps before it are identity copies upstream.
    fn next_active(&self, text: &str, from: u32) -> Option<u32> {
        let mut best = u32::MAX;
        let mut scan_from = 0;
        while let Some((start, end)) = find_var_ref(text, scan_from) {
            if let Some(&i) = self.by_name.get(&text[start + 6..end - 1])
                && i >= from
            {
                if i == from {
                    return Some(i);
                }
                best = best.min(i);
            }
            scan_from = end;
        }
        if self.rewrites_keys {
            let found_from = for_each_custom_property_key(text, |name| {
                if let Some(&i) = self.by_name.get(name)
                    && i >= from
                    && i < best
                    && self.consts[i as usize].key_rewrite.is_some()
                {
                    best = i;
                    if i == from {
                        return ControlFlow::Break(());
                    }
                }
                ControlFlow::Continue(())
            });
            if found_from.is_break() {
                return Some(from);
            }
        }
        for &i in &self.irregular {
            if i >= best {
                break;
            }
            if i >= from && self.consts[i as usize].touches(text) {
                best = i;
                break;
            }
        }
        (best != u32::MAX).then_some(best)
    }

    // How many constants at or after `from` the indexed loop may still have
    // to step through for `text`: the ones its references and rewritable
    // keys name, each once however often it is named, and every irregular
    // one, which costs that loop the same search the plain loop makes.
    fn live_count(&self, text: &str, from: u32) -> usize {
        let mut named: Vec<u32> = Vec::new();
        let mut scan_from = 0;
        while let Some((start, end)) = find_var_ref(text, scan_from) {
            if let Some(&i) = self.by_name.get(&text[start + 6..end - 1])
                && i >= from
            {
                named.push(i);
            }
            scan_from = end;
        }
        if self.rewrites_keys {
            let _ = for_each_custom_property_key(text, |name| {
                if let Some(&i) = self.by_name.get(name)
                    && i >= from
                    && self.consts[i as usize].key_rewrite.is_some()
                {
                    named.push(i);
                }
                ControlFlow::Continue(())
            });
        }
        named.sort_unstable();
        named.dedup();
        named.len() + self.irregular.len() - self.irregular.partition_point(|&i| i < from)
    }
}

// Finding a rule's next active constant rescans its text, a name lookup per
// reference and key in it, so a rule that thousands of constants rewrite
// would pay steps x references in lookups. Upstream's plain loop pays a
// substring search per constant instead, whether the rule names it or not:
// cheaper for that rule, and far dearer for one that names a hundred
// constants of twenty thousand. After this many indexed steps, and at every
// doubling of them, the constants still ahead are weighed against the ones
// the rule still names, and the rest run as the plain loop only where it is
// the cheaper of the two.
const MAX_INDEXED_STEPS: u32 = 32;

// parity: the per-rule constsMap loop. Upstream runs every constant's step in
// order over the running text; a step that matches nothing copies the text, so
// only the steps `next_active` finds are run, each exactly as upstream does.
/// `None` when no const reference matched: callers keep sharing the rule's
/// Arc'd text instead of materializing an identical String.
fn substitute_consts(text: &str, consts: &ConstTable) -> Option<String> {
    substitute_consts_capped(text, consts, MAX_INDEXED_STEPS)
}

// `substitute_consts` with the number of indexed steps it takes before it
// first weighs running the remaining constants in sequence; the result is the
// same for any cap.
fn substitute_consts_capped(text: &str, consts: &ConstTable, max_indexed: u32) -> Option<String> {
    if consts.is_empty() {
        return None;
    }
    let mut out: Option<String> = None;
    let mut from = 0;
    let mut indexed = 0;
    let mut check_at = max_indexed;
    while let Some(i) = consts.next_active(out.as_deref().unwrap_or(text), from) {
        if indexed == check_at {
            let live = consts.live_count(out.as_deref().unwrap_or(text), i);
            // The sequence searches for every constant ahead, an indexed step
            // rescans the text for each one still named; the rescan also
            // walks the declaration keys when constants rewrite them.
            let factor = if consts.rewrites_keys { 16 } else { 8 };
            if consts.consts.len() - i as usize <= factor * live.max(1) {
                // The steps before `i` match nothing, so the sequence starts
                // there.
                for c in &consts.consts[i as usize..] {
                    c.substitute(text, &mut out);
                }
                break;
            }
            check_at = check_at.saturating_mul(2).max(1);
        }
        consts.consts[i as usize].substitute(text, &mut out);
        from = i + 1;
        indexed += 1;
    }
    out
}

// JS GetSubstitution for string-pattern replace/replaceAll: only $$, $&, $`, $'
// are active (no capture groups exist).
fn js_substitution(out: &mut Vec<u8>, full: &str, pos: usize, matched: &str, replacement: &str) {
    let rep = replacement.as_bytes();
    let mut i = 0;
    while i < rep.len() {
        if rep[i] == b'$' && i + 1 < rep.len() {
            match rep[i + 1] {
                b'$' => {
                    out.push(b'$');
                    i += 2;
                    continue;
                }
                b'&' => {
                    out.extend_from_slice(matched.as_bytes());
                    i += 2;
                    continue;
                }
                b'`' => {
                    out.extend_from_slice(&full.as_bytes()[..pos]);
                    i += 2;
                    continue;
                }
                b'\'' => {
                    out.extend_from_slice(&full.as_bytes()[pos + matched.len()..]);
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        out.push(rep[i]);
        i += 1;
    }
}

fn js_replace_all(s: &str, needle: &str, replacement: &str) -> String {
    debug_assert!(!needle.is_empty());
    let mut out = Vec::with_capacity(s.len());
    let mut pos = 0;
    while let Some(off) = s[pos..].find(needle) {
        let at = pos + off;
        out.extend_from_slice(&s.as_bytes()[pos..at]);
        js_substitution(&mut out, s, at, needle, replacement);
        pos = at + needle.len();
    }
    out.extend_from_slice(&s.as_bytes()[pos..]);
    String::from_utf8(out).expect("byte-splices preserve UTF-8")
}

// Rules that take neither the specificity bump nor a direction ancestor.
fn is_selector_exempt(rule: &str) -> bool {
    rule.starts_with("@keyframes") || rule.starts_with("@position-try")
}

// :not(#\#) polyfill; inserted before the first '::' when present, else before
// the last '{'; @keyframes and @position-try are exempt (@property etc. are not).
fn push_specificity_level(out: &mut String, selector: &str, index: usize) {
    if is_selector_exempt(selector) {
        out.push_str(selector);
        return;
    }
    push_split(out, selector, specificity_split(selector), index);
}

fn specificity_split(selector: &str) -> usize {
    match selector.find("::") {
        Some(i) => i,
        None => match selector.rfind('{') {
            Some(i) => i,
            // lastIndexOf miss → -1 → JS slice(0,-1) / slice(-1) split before the
            // final character.
            None => selector.char_indices().last().map(|(i, _)| i).unwrap_or(0),
        },
    }
}

fn push_split(out: &mut String, selector: &str, split_at: usize, index: usize) {
    out.push_str(&selector[..split_at]);
    for _ in 0..index {
        out.push_str(":not(#\\#)");
    }
    out.push_str(&selector[split_at..]);
}

// Split point of a rule that renders as plain slices (no rtl, no theme pair,
// not exempt from the bump); None routes the rule through push_chunk.
fn plain_split(ltr: &str, rtl: Option<&str>) -> Option<u32> {
    if rtl.is_some_and(|t| !t.is_empty()) || has_theme_pair_hint(ltr) || is_selector_exempt(ltr) {
        return None;
    }
    u32::try_from(specificity_split(ltr)).ok()
}

// `", ."` is a literal every theme-pair match contains.
fn has_theme_pair_hint(text: &str) -> bool {
    let bytes = text.as_bytes();
    memchr::memchr_iter(b',', bytes).any(|comma| bytes[comma + 1..].starts_with(b" ."))
}

// /\.([a-zA-Z0-9]+), \.([a-zA-Z0-9]+):root/g → '.$1.$1, .$1.$1:root' — the
// second class is intentionally overwritten by the first, as upstream does.
fn double_theme_classes(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'.'
            && let Some((end, class_end)) = match_theme_pair(bytes, i)
        {
            let class = &s[i + 1..class_end];
            out.extend_from_slice(format!(".{class}.{class}, .{class}.{class}:root").as_bytes());
            i = end;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).expect("ASCII-anchored splices preserve UTF-8")
}

fn match_theme_pair(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut j = start + 1;
    while j < bytes.len() && bytes[j].is_ascii_alphanumeric() {
        j += 1;
    }
    if j == start + 1 {
        return None;
    }
    let class_end = j;
    if !bytes[j..].starts_with(b", .") {
        return None;
    }
    j += 3;
    let second_start = j;
    while j < bytes.len() && bytes[j].is_ascii_alphanumeric() {
        j += 1;
    }
    if j == second_start || !bytes[j..].starts_with(b":root") {
        return None;
    }
    Some((j + 5, class_end))
}

// Re-exported so callers can use the default order with a custom comparator.
pub use crate::jsrt::default_locale_cmp;

fn class_hash64(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// Hash values are precomputed 64-bit class hashes; pass them through verbatim.
#[derive(Default)]
struct IdentityHasher(u64);

impl std::hash::Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8) | u64::from(b);
        }
    }
    fn write_u64(&mut self, x: u64) {
        self.0 = x;
    }
}

struct PreparedRule {
    class_name: Arc<str>,
    class_hash: u64,
    // The sort priority: the rule's own, re-priced for a constant at-rule wrapper.
    priority: f64,
    // key and width are read from the const-substituted ltr.
    key: RuleKey,
    width: Option<Arc<WidthSortKey>>,
    // ltr/rtl carry the const substitutions; unchanged text shares the rule's Arc.
    ltr: Arc<str>,
    rtl: Option<Arc<str>>,
    plain_split: Option<u32>,
}

impl PreparedRule {
    fn cmp_key(&self, other: &PreparedRule) -> Ordering {
        self.key
            .cmp(&other.key, self.width.as_deref(), other.width.as_deref())
    }
}

struct FilePrep {
    rules: PreparedRules,
    const_decls: Vec<ConstDecl>,
    // safe = every priority finite: the precondition under which sorted-by-key
    // order equals the stable sort. Non-finite priorities keep the one-shot
    // sort.
    safe: bool,
    logical_float: bool,
}

impl FilePrep {
    // Sized for exactly `rules` prepared rules and `consts` declarations: a
    // file's preparation lives as long as the file does, so the slack a
    // growing Vec leaves behind would be held for good.
    fn empty(shared: bool, rules: usize, consts: usize) -> FilePrep {
        FilePrep {
            rules: if shared {
                PreparedRules::Shared(Vec::with_capacity(rules))
            } else {
                PreparedRules::Inline(Vec::with_capacity(rules))
            },
            const_decls: Vec::with_capacity(consts),
            safe: true,
            logical_float: false,
        }
    }
}

struct SharedPreparedRule {
    source: StylexRule,
    prepared: PreparedRule,
}

enum PreparedRules {
    Inline(Vec<PreparedRule>),
    Shared(Vec<Arc<SharedPreparedRule>>),
}

impl PreparedRules {
    fn len(&self) -> usize {
        match self {
            Self::Inline(rules) => rules.len(),
            Self::Shared(rules) => rules.len(),
        }
    }

    fn iter(&self) -> impl Iterator<Item = &PreparedRule> {
        (0..self.len()).map(|i| &self[i])
    }
}

impl std::ops::Index<usize> for PreparedRules {
    type Output = PreparedRule;

    fn index(&self, i: usize) -> &Self::Output {
        match self {
            Self::Inline(rules) => &rules[i],
            Self::Shared(rules) => &rules[i].prepared,
        }
    }
}

// The preparations files share, by class hash, and how many are alive: one
// per distinct rule, however many files repeat it.
#[derive(Default)]
struct PreparedPool {
    buckets: HashMap<u64, Vec<Weak<SharedPreparedRule>>, BuildHasherDefault<IdentityHasher>>,
    live: usize,
}

impl PreparedPool {
    fn share(
        &mut self,
        r: &StylexRule,
        legacy: bool,
        consts: &ConstTable,
    ) -> Arc<SharedPreparedRule> {
        let bucket = self.buckets.entry(class_hash64(&r.class_name)).or_default();
        let known = bucket
            .iter()
            .filter_map(Weak::upgrade)
            .find(|p| p.source == *r);
        known.unwrap_or_else(|| {
            let shared = Arc::new(SharedPreparedRule {
                source: r.clone(),
                prepared: prepare_rule(r, legacy, consts),
            });
            bucket.push(Arc::downgrade(&shared));
            self.live += 1;
            shared
        })
    }

    // Drops one file's preparations and forgets those no other file holds, so
    // an edit costs its own rules instead of a sweep of the pool.
    fn release(&mut self, rules: Vec<Arc<SharedPreparedRule>>) {
        for rule in rules {
            if Arc::strong_count(&rule) > 1 {
                continue;
            }
            if let Entry::Occupied(mut bucket) = self.buckets.entry(rule.prepared.class_hash) {
                bucket
                    .get_mut()
                    .retain(|shared| !std::ptr::eq(shared.as_ptr(), Arc::as_ptr(&rule)));
                if bucket.get().is_empty() {
                    bucket.remove();
                }
            }
            self.live -= 1;
        }
    }
}

fn retire(pool: &mut Option<PreparedPool>, prep: FilePrep) {
    if let (Some(pool), PreparedRules::Shared(rules)) = (pool, prep.rules) {
        pool.release(rules);
    }
}

fn prefer_shared_preparation(files: &BTreeMap<String, Vec<StylexRule>>) -> bool {
    let mut seen = FxHashSet::default();
    let mut total = 0;
    let hasher = crate::fxhash::FxBuildHasher::default();
    for rule in files.values().flatten().filter(|r| !is_const_rule(r)) {
        total += 1;
        seen.insert(hasher.hash_one((
            &rule.class_name,
            &rule.ltr,
            &rule.rtl,
            rule.priority.to_bits(),
        )));
    }
    total >= 256 && seen.len() * 2 <= total
}

fn prepare_rule(r: &StylexRule, legacy: bool, consts: &ConstTable) -> PreparedRule {
    let ltr = substitute_consts(&r.ltr, consts)
        .map(Arc::from)
        .unwrap_or_else(|| r.ltr.clone());
    let rtl = r.rtl.as_ref().map(|t| {
        substitute_consts(t, consts)
            .map(Arc::from)
            .unwrap_or_else(|| t.clone())
    });
    PreparedRule {
        class_name: r.class_name.clone(),
        class_hash: class_hash64(&r.class_name),
        priority: adjust_const_at_rule_priority(&r.ltr, r.priority, consts),
        key: RuleKey::for_text(&r.class_name, &ltr, legacy),
        width: if legacy { None } else { width_sort_key(&ltr) },
        plain_split: plain_split(&ltr, rtl.as_deref()),
        ltr,
        rtl,
    }
}

fn prepare_file(
    rules: &[StylexRule],
    legacy: bool,
    consts: &ConstTable,
    pool: &mut Option<PreparedPool>,
) -> FilePrep {
    let const_count = rules.iter().filter(|r| is_const_rule(r)).count();
    let mut prep = FilePrep::empty(pool.is_some(), rules.len() - const_count, const_count);
    for r in rules {
        if is_const_rule(r) {
            prep.const_decls.push(const_decl(r));
            continue;
        }
        match &mut prep.rules {
            PreparedRules::Inline(rules) => rules.push(prepare_rule(r, legacy, consts)),
            PreparedRules::Shared(rules) => rules.push(
                pool.as_mut()
                    .expect("shared preparation pool")
                    .share(r, legacy, consts),
            ),
        }
        let prepared = &prep.rules[prep.rules.len() - 1];
        prep.safe &= prepared.priority.is_finite();
        if !prep.safe {
            // No caller keeps an unsafe preparation, or reads more of it.
            return prep;
        }
        prep.logical_float |= text_has_logical_float(&prepared.ltr, prepared.rtl.as_deref());
    }
    prep
}

// Strict total order = comparator order + the (file, seq) input position; with
// safe files this reproduces the one-shot stable sort exactly.
fn entry_cmp(files: &[FilePrep], a: (u32, u32), b: (u32, u32)) -> Ordering {
    let pa = &files[a.0 as usize].rules[a.1 as usize];
    let pb = &files[b.0 as usize].rules[b.1 as usize];
    compare_priorities(pa.priority, pb.priority)
        .unwrap_or_else(|| pa.cmp_key(pb))
        .then_with(|| a.cmp(&b))
}

type RuleOwners = FxHashMap<Arc<str>, Vec<(u32, u32)>>;

struct IncrState {
    cfg: AssembleConfig,
    // Files set or removed since this state was last brought up to date.
    dirty: BTreeSet<String>,
    consts: Vec<ConstDecl>,
    table: Arc<ConstTable>,
    paths: Vec<String>,
    files: Vec<FilePrep>,
    pool: Option<PreparedPool>,
    // Print order as (file rank, rule seq). With `owners`, one entry per
    // class: the first of its owners.
    sorted: Vec<(u32, u32)>,
    owners: Option<RuleOwners>,
    // The rule count when inline storage was last weighed against sharing.
    weighed_at: usize,
    // Size of the last render: the capacity of the next.
    rendered_len: usize,
    scratch: RenderScratch,
}

// A config the splice path cannot serve. Rebuilding its state is pointless
// until the input that ruled it out changes, so it is remembered instead of
// being retried on every emit.
struct Blocked {
    cfg: AssembleConfig,
    on: Blocker,
}

enum Blocker {
    // This file holds a rule outside the safe preconditions, as prepared
    // against these constants: enough to tell, when the file changes, whether
    // it still does.
    File {
        path: String,
        consts: Arc<ConstTable>,
    },
    // The constants do not resolve.
    Consts,
}

const MAX_INCR_STATES: usize = 4;

// A state no emit has asked for collects the path of every file touched in
// the meantime. Once that backlog outgrows the registry and the files the
// state holds (and this floor, which keeps a small registry from dropping
// states as it drains), bringing the state up to date costs no less than
// building it, and it is dropped. The state's own files count because a
// batch that removes most of the registry shrinks it under its own backlog,
// and the state the next emit asks for still splices those removals for less
// than a rebuild.
const MAX_IDLE_DIRTY: usize = 16;

// Incremental assembly for the HMR path (design-core.md §7): file-keyed rule
// sets, canonical emission order = BTreeMap file order, memoized full assemble.
#[derive(Default)]
pub struct RuleRegistry {
    files: BTreeMap<String, Vec<StylexRule>>,
    generation: u64,
    cache: Option<(u64, AssembleConfig, Result<String, AssembleError>)>,
    incr: Vec<IncrState>,
    blocked: Vec<Blocked>,
}

impl RuleRegistry {
    pub fn new() -> Self {
        RuleRegistry::default()
    }

    pub fn set_file_rules(&mut self, file: &str, rules: Vec<StylexRule>) {
        let old = self.files.get(file);
        if old.is_some_and(|old| *old == rules) {
            return;
        }
        unblock(
            &mut self.blocked,
            file,
            old.map_or(&[], Vec::as_slice),
            &rules,
        );
        self.files.insert(file.to_string(), rules);
        self.touch(file);
    }

    pub fn remove_file(&mut self, file: &str) -> bool {
        let Some(old) = self.files.remove(file) else {
            return false;
        };
        unblock(&mut self.blocked, file, &old, &[]);
        self.touch(file);
        true
    }

    fn touch(&mut self, file: &str) {
        let floor = self.files.len().max(MAX_IDLE_DIRTY);
        self.incr.retain_mut(|state| {
            state.dirty.insert(file.to_string());
            state.dirty.len() <= floor.max(state.paths.len())
        });
        self.generation += 1;
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Every file's rules, in path order.
    pub fn files(&self) -> impl Iterator<Item = (&str, &[StylexRule])> {
        self.files
            .iter()
            .map(|(file, rules)| (file.as_str(), rules.as_slice()))
    }

    // The one-shot-equivalent rule list: files in path order, rules in file order.
    pub fn all_rules(&self) -> Vec<StylexRule> {
        self.files.values().flatten().cloned().collect()
    }

    pub fn emit(&mut self, cfg: &AssembleConfig) -> Result<String, AssembleError> {
        if let Some((generation, cached_cfg, result)) = &self.cache
            && *generation == self.generation
            && cached_cfg == cfg
        {
            return result.clone();
        }
        let spliced = if self.blocked.iter().any(|blocked| blocked.cfg == *cfg) {
            None
        } else {
            match self.try_incremental(cfg) {
                Ok(css) => Some(css),
                // The splice met the blocker a rebuild would stop at.
                Err(Some(on)) => {
                    self.block(cfg, on);
                    None
                }
                Err(None) => {
                    self.rebuild_incr_state(cfg);
                    self.try_incremental(cfg).ok()
                }
            }
        };
        let result = match spliced {
            Some(css) => {
                // Debug cross-check: the incremental path must match one-shot output.
                #[cfg(debug_assertions)]
                {
                    let full = assemble(&self.all_rules(), cfg);
                    debug_assert_eq!(
                        full.as_deref(),
                        Ok(css.as_str()),
                        "incremental emit diverged from one-shot assemble"
                    );
                }
                Ok(css)
            }
            None => {
                let rule_count = self.files.values().map(Vec::len).sum();
                assemble_impl(self.files.values().flatten(), rule_count, cfg, None)
            }
        };
        self.cache = Some((self.generation, cfg.clone(), result.clone()));
        result
    }

    // The fast path: bring the per-config state up to date with the files set,
    // added and removed since it last ran, then print its sort order. Err =
    // fall back to the one-shot path: with the blocker when the splice found
    // what rules the config out, else with nothing a rebuild would not have
    // to find for itself.
    fn try_incremental(&mut self, cfg: &AssembleConfig) -> Result<String, Option<Blocker>> {
        let Self { files, incr, .. } = self;
        let at = incr.iter().position(|s| s.cfg == *cfg).ok_or(None)?;
        if !incr[at].dirty.is_empty()
            && let Err(blocker) = incr[at].splice(files)
        {
            // A splice that gave up half-way leaves nothing to print from.
            incr.remove(at);
            return Err(blocker);
        }
        if files.values().all(|rules| rules.is_empty()) {
            return Ok(String::new());
        }
        Ok(render(&mut incr[at]))
    }

    // Full rebuild of the per-config state; when the rule set is outside the
    // safe preconditions (or consts fail to resolve) the config is blocked
    // instead, until `unblock` sees the reason change.
    fn rebuild_incr_state(&mut self, cfg: &AssembleConfig) {
        self.incr.retain(|s| s.cfg != *cfg);
        match IncrState::build(&self.files, cfg) {
            Ok(state) => {
                if self.incr.len() >= MAX_INCR_STATES {
                    self.incr.remove(0);
                }
                self.incr.push(state);
            }
            Err(on) => self.block(cfg, on),
        }
    }

    fn block(&mut self, cfg: &AssembleConfig, on: Blocker) {
        if self.blocked.len() >= MAX_INCR_STATES {
            self.blocked.remove(0);
        }
        self.blocked.push(Blocked {
            cfg: cfg.clone(),
            on,
        });
    }
}

// Whether `rules` declare exactly the constants `decls`, in that order.
fn declares(rules: &[StylexRule], decls: &[ConstDecl]) -> bool {
    let mut decls = decls.iter();
    const_decls(rules).all(|decl| decls.next() == Some(&decl)) && decls.next().is_none()
}

// `file` went from `old` to `new`: a config it was blocking gets another try
// once the file is safe. A constant reaches the text and priority of every
// file's rules, so a change to the constants retries every config.
fn unblock(blocked: &mut Vec<Blocked>, file: &str, old: &[StylexRule], new: &[StylexRule]) {
    if blocked.is_empty() {
        return;
    }
    let consts_changed = !const_decls(old).eq(const_decls(new));
    blocked.retain(|blocked| match &blocked.on {
        Blocker::Consts => !consts_changed,
        Blocker::File { .. } if consts_changed => false,
        Blocker::File { path, .. } if path != file => true,
        // The blocker changed around the constants it was prepared against
        // (a change to them drops the entry above): preparing it alone
        // answers what a retry would prepare every file before it to learn.
        Blocker::File { consts, .. } => {
            let legacy = blocked.cfg.use_legacy_classnames_sort;
            !prepare_file(new, legacy, consts, &mut None).safe
        }
    });
}

// One file of the layout a splice leaves behind, in path order.
enum Slot {
    // An old slot that stays as it is.
    Kept(usize),
    // An old slot taking the incoming preparation of that index.
    Edited(usize, usize),
    // A new slot for the incoming preparation of that index.
    Added(usize),
}

fn splice_layout(
    paths: &[String],
    changed: &[bool],
    incoming: &[(&String, FilePrep)],
) -> Vec<Slot> {
    let mut slots = Vec::with_capacity(paths.len() + incoming.len());
    let mut next = 0;
    for (rank, path) in paths.iter().enumerate() {
        while next < incoming.len() && incoming[next].0 < path {
            slots.push(Slot::Added(next));
            next += 1;
        }
        if !changed[rank] {
            slots.push(Slot::Kept(rank));
        } else if next < incoming.len() && incoming[next].0 == path {
            slots.push(Slot::Edited(rank, next));
            next += 1;
        }
    }
    slots.extend((next..incoming.len()).map(Slot::Added));
    slots
}

impl IncrState {
    // Err = the rule set cannot be spliced, and what has to change first.
    fn build(
        files: &BTreeMap<String, Vec<StylexRule>>,
        cfg: &AssembleConfig,
    ) -> Result<IncrState, Blocker> {
        let raw = files.values().flat_map(|rules| const_decls(rules));
        let consts = collect_resolved_consts(raw).map_err(|_| Blocker::Consts)?;
        let table = Arc::new(prepare_consts(&consts));
        let prepare_all = |pool: &mut Option<PreparedPool>| -> Result<Vec<FilePrep>, Blocker> {
            let mut preps = Vec::with_capacity(files.len());
            for (path, rules) in files {
                let prep = prepare_file(rules, cfg.use_legacy_classnames_sort, &table, pool);
                if !prep.safe {
                    return Err(Blocker::File {
                        path: path.clone(),
                        consts: Arc::clone(&table),
                    });
                }
                preps.push(prep);
            }
            Ok(preps)
        };
        let mut pool = prefer_shared_preparation(files).then(PreparedPool::default);
        let mut preps = prepare_all(&mut pool)?;
        let rule_count: usize = preps.iter().map(|file| file.rules.len()).sum();
        if pool.as_ref().is_some_and(|pool| pool.live * 2 > rule_count) {
            // The pool tells rules apart that the estimate did not: a splice
            // would refuse this state at once, so it is built inline instead.
            pool = None;
            preps = prepare_all(&mut pool)?;
        }

        let entries = preps.iter().enumerate().flat_map(|(rank, file)| {
            (0..file.rules.len()).map(move |seq| (rank as u32, seq as u32))
        });
        let owners = pool.as_ref().and_then(|_| {
            let mut owners = RuleOwners::default();
            for entry in entries.clone() {
                insert_owner(&mut owners, &preps, entry)?;
            }
            Some(owners)
        });
        let rule = |(rank, seq): (u32, u32)| {
            let prepared = &preps[rank as usize].rules[seq as usize];
            (prepared.priority, &prepared.key, prepared.width.as_deref())
        };
        let sorted = match &owners {
            Some(owners) => sort_finite(owners.values().map(|entries| entries[0]), rule),
            None => sort_finite(entries, rule),
        };
        Ok(IncrState {
            cfg: cfg.clone(),
            dirty: BTreeSet::new(),
            consts,
            table,
            paths: files.keys().cloned().collect(),
            files: preps,
            pool,
            sorted,
            owners,
            weighed_at: rule_count,
            rendered_len: 0,
            scratch: RenderScratch::default(),
        })
    }

    // Brings the state up to date with the registry: re-prepares the files set
    // since the last emit, drops the removed ones, slots in the added ones and
    // splices their rules into the sort order. Err = the state can no longer
    // stand in for a full assembly: with the blocker when a dirty file is
    // what rules the splice path out, else the caller rebuilds the state.
    fn splice(&mut self, files: &BTreeMap<String, Vec<StylexRule>>) -> Result<(), Option<Blocker>> {
        let legacy = self.cfg.use_legacy_classnames_sort;
        let dirty = std::mem::take(&mut self.dirty);

        // The old slots that are edited or removed, and the preparations
        // coming in (edited and added files), in path order.
        let mut changed = vec![false; self.files.len()];
        let mut incoming: Vec<(&String, FilePrep)> = Vec::with_capacity(dirty.len());
        let mut layout_changed = false;
        let mut consts_touched = false;
        for path in &dirty {
            let old = self.paths.binary_search(path).ok();
            let old_decls: &[ConstDecl] = old.map_or(&[], |rank| &self.files[rank].const_decls);
            match files.get(path) {
                Some(rules) => {
                    let prep = prepare_file(rules, legacy, &self.table, &mut self.pool);
                    if !prep.safe {
                        // With the constants as the state has them, this is
                        // the file a rebuild would stop at, prepared against
                        // an equal table: every file before it is safe. That
                        // holds only if no dirty file, this one and the ones
                        // not looked at yet included, declares other
                        // constants than it did.
                        let consts_kept = !consts_touched
                            && dirty.range::<String, _>(path..).all(|path| {
                                let rules = files.get(path).map_or(&[][..], Vec::as_slice);
                                let old = self.paths.binary_search(path).ok();
                                declares(
                                    rules,
                                    old.map_or(&[], |rank| &self.files[rank].const_decls),
                                )
                            });
                        return Err(consts_kept.then(|| Blocker::File {
                            path: path.clone(),
                            consts: Arc::clone(&self.table),
                        }));
                    }
                    consts_touched |= prep.const_decls != old_decls;
                    layout_changed |= old.is_none();
                    incoming.push((path, prep));
                }
                None => {
                    if old.is_none() {
                        continue;
                    }
                    consts_touched |= !old_decls.is_empty();
                    layout_changed = true;
                }
            }
            if let Some(rank) = old {
                changed[rank] = true;
            }
        }
        let layout = if layout_changed || consts_touched {
            splice_layout(&self.paths, &changed, &incoming)
        } else {
            Vec::new()
        };

        // The rules were prepared against the state's constants: the splice
        // stands only if the dirty files leave the resolved constants as they
        // were. Files that declare the constants they declared before (most
        // edits: none) cannot change them, and skip the resolution.
        if consts_touched {
            let raw = layout.iter().flat_map(|slot| match *slot {
                Slot::Kept(rank) => self.files[rank].const_decls.iter().cloned(),
                Slot::Edited(_, at) | Slot::Added(at) => incoming[at].1.const_decls.iter().cloned(),
            });
            if collect_resolved_consts(raw).map_err(|_| None)? != self.consts {
                return Err(None);
            }
        }

        // Drop the sort entries of the edited and removed slots. With owners
        // an entry stands for a class, so every class those slots or the
        // incoming files name gives up its entry and re-elects one below.
        let mut classes: FxHashSet<Arc<str>> = FxHashSet::default();
        match &mut self.owners {
            Some(owners) => {
                let outgoing = changed
                    .iter()
                    .zip(&self.files)
                    .filter_map(|(changed, file)| changed.then_some(file));
                for file in outgoing.chain(incoming.iter().map(|(_, prep)| prep)) {
                    classes.extend(file.rules.iter().map(|rule| rule.class_name.clone()));
                }
                let mut doomed = Vec::with_capacity(classes.len());
                for class in &classes {
                    if let Some(entries) = owners.get(class) {
                        let at = self
                            .sorted
                            .binary_search_by(|&entry| entry_cmp(&self.files, entry, entries[0]));
                        doomed.push(at.map_err(|_| None)?);
                    }
                }
                doomed.sort_unstable();
                remove_positions(&mut self.sorted, &doomed);
                for class in &classes {
                    if let Some(entries) = owners.get_mut(class) {
                        entries.retain(|&(rank, _)| !changed[rank as usize]);
                    }
                }
            }
            None => self.sorted.retain(|&(rank, _)| !changed[rank as usize]),
        }

        // Added and removed files move the ranks behind them. The move keeps
        // rank order, so the surviving entries stay sorted as they are.
        if layout_changed {
            let mut old_paths =
                std::mem::replace(&mut self.paths, Vec::with_capacity(layout.len()));
            let mut old_files: Vec<Option<FilePrep>> =
                std::mem::replace(&mut self.files, Vec::with_capacity(layout.len()))
                    .into_iter()
                    .map(Some)
                    .collect();
            let mut new_rank = vec![u32::MAX; old_paths.len()];
            for slot in &layout {
                match *slot {
                    Slot::Kept(rank) | Slot::Edited(rank, _) => {
                        new_rank[rank] = self.paths.len() as u32;
                        self.paths.push(std::mem::take(&mut old_paths[rank]));
                        self.files.push(old_files[rank].take().ok_or(None)?);
                    }
                    Slot::Added(at) => {
                        self.paths.push(incoming[at].0.clone());
                        self.files.push(FilePrep::empty(self.pool.is_some(), 0, 0));
                    }
                }
            }
            for removed in old_files.into_iter().flatten() {
                retire(&mut self.pool, removed);
            }
            let owned = self
                .owners
                .iter_mut()
                .flat_map(|owners| owners.values_mut());
            for entry in self.sorted.iter_mut().chain(owned.flatten()) {
                entry.0 = new_rank[entry.0 as usize];
            }
        }

        // Install the incoming preparations and splice their entries in.
        let mut fresh: Vec<(u32, u32)> = Vec::new();
        for (path, prep) in incoming {
            let rank = self.paths.binary_search(path).map_err(|_| None)?;
            let old = std::mem::replace(&mut self.files[rank], prep);
            retire(&mut self.pool, old);
            for seq in 0..self.files[rank].rules.len() {
                let entry = (rank as u32, seq as u32);
                match &mut self.owners {
                    Some(owners) => insert_owner(owners, &self.files, entry).ok_or(None)?,
                    None => fresh.push(entry),
                }
            }
        }
        if let Some(owners) = &mut self.owners {
            for class in classes {
                let Entry::Occupied(mut entries) = owners.entry(class) else {
                    return Err(None);
                };
                if entries.get().is_empty() {
                    entries.remove();
                } else {
                    entries.get_mut().sort_unstable();
                    fresh.push(entries.get()[0]);
                }
            }
        }
        if !fresh.is_empty() {
            fresh.sort_unstable_by(|&a, &b| entry_cmp(&self.files, a, b));
            merge_sorted(&self.files, &mut self.sorted, &fresh);
        }

        let rule_count: usize = self.files.iter().map(|file| file.rules.len()).sum();
        match &self.pool {
            // Too little repetition left to pay for the sharing.
            Some(pool) if pool.live * 2 > rule_count => return Err(None),
            Some(_) => {}
            // Files added since the state was built can bring the repetition
            // sharing wants; weighing it again at each doubling keeps the
            // check amortized.
            None if rule_count >= 2 * self.weighed_at.max(128) => {
                if prefer_shared_preparation(files) {
                    return Err(None);
                }
                self.weighed_at = rule_count;
            }
            None => {}
        }
        Ok(())
    }
}

fn remove_positions<T>(items: &mut Vec<T>, ascending: &[usize]) {
    let mut doomed = ascending.iter().copied().peekable();
    let mut at = 0;
    items.retain(|_| {
        let remove = doomed.next_if_eq(&at).is_some();
        at += 1;
        !remove
    });
}

fn insert_owner(owners: &mut RuleOwners, files: &[FilePrep], entry: (u32, u32)) -> Option<()> {
    let rule = &files[entry.0 as usize].rules[entry.1 as usize];
    let entries = owners.entry(rule.class_name.clone()).or_default();
    if let Some(&(rank, seq)) = entries.first() {
        let previous = &files[rank as usize].rules[seq as usize];
        // Conflicting class definitions retain upstream's first-slot/last-value semantics.
        if previous.priority != rule.priority
            || previous.cmp_key(rule) != Ordering::Equal
            || previous.ltr != rule.ltr
            || previous.rtl != rule.rtl
        {
            return None;
        }
    }
    entries.push(entry);
    Some(())
}

// Merges the sorted `fresh` entries into the sorted `kept`, in place and from
// the back: each fresh entry is placed by bisection, the runs between them
// move once.
fn merge_sorted(files: &[FilePrep], kept: &mut Vec<(u32, u32)>, fresh: &[(u32, u32)]) {
    let mut read_end = kept.len();
    let mut write_end = kept.len() + fresh.len();
    kept.resize(write_end, (0, 0));
    for &candidate in fresh.iter().rev() {
        let at =
            kept[..read_end].partition_point(|&e| entry_cmp(files, e, candidate) == Ordering::Less);
        let run = read_end - at;
        kept.copy_within(at..read_end, write_end - run);
        write_end -= run + 1;
        kept[write_end] = candidate;
        read_end = at;
    }
}

// First slot inline: an empty Vec never allocates, so only a real 64-bit hash
// collision costs a heap allocation.
type SlotMap = HashMap<u64, (u32, Vec<u32>), BuildHasherDefault<IdentityHasher>>;

// One priority band of the print order as positions into the printed
// entries: `start..bare_end` prints bare, `bare_end..end` inside the band's
// @layer. With layers off the whole band is bare.
struct Band {
    start: usize,
    bare_end: usize,
    end: usize,
}

// Buffers a render fills and the next one reuses.
#[derive(Default)]
struct RenderScratch {
    printed: Vec<(u32, u32)>,
    bands: Vec<Band>,
    slot_map: SlotMap,
}

fn render(state: &mut IncrState) -> String {
    let IncrState {
        cfg,
        files,
        sorted,
        owners,
        rendered_len,
        scratch,
        ..
    } = state;
    let RenderScratch {
        printed,
        bands,
        slot_map,
    } = scratch;
    let (use_layers_on, _, _, layer_prefix) = layer_parts(cfg);
    let rule = |(rank, seq): (u32, u32)| &files[rank as usize].rules[seq as usize];
    let band_of = |entry: (u32, u32)| (rule(entry).priority / 1000.0).floor();

    bands.clear();
    let printed: &[(u32, u32)] = if owners.is_some() {
        // One entry per class: nothing to dedupe, and the priorities are
        // finite and ascending, so bands and their parts are runs of the sort
        // order that bisection finds without visiting every rule.
        let mut start = 0;
        while start < sorted.len() {
            let band = band_of(sorted[start]);
            let entries = &sorted[start..];
            let end = start + entries.partition_point(|&entry| band_of(entry) == band);
            let entries = &sorted[start..end];
            bands.push(if use_layers_on {
                // Upstream prints `pri === 0` bare and `pri > 0` layered; a
                // negative priority is in neither part.
                let bare = entries.partition_point(|&entry| rule(entry).priority < 0.0);
                let layered = entries.partition_point(|&entry| rule(entry).priority <= 0.0);
                Band {
                    start: start + bare,
                    bare_end: start + layered,
                    end,
                }
            } else {
                Band {
                    start,
                    bare_end: end,
                    end,
                }
            });
            start = end;
        }
        sorted
    } else {
        printed.clear();
        let mut last_band = 0.0f64;
        let mut in_layered = false;
        for &entry in sorted.iter() {
            let p = rule(entry);
            let band = (p.priority / 1000.0).floor();
            if bands.is_empty() || band != last_band {
                bands.push(Band {
                    start: printed.len(),
                    bare_end: printed.len(),
                    end: printed.len(),
                });
                slot_map.clear();
                last_band = band;
                in_layered = false;
            }
            // Priorities are finite and ascending here, so a band's priority-0
            // rules all precede its layered ones: one dedupe scope per part.
            match BandPart::of(use_layers_on, p.priority) {
                Some(BandPart::Layered) if !in_layered => {
                    slot_map.clear();
                    in_layered = true;
                }
                Some(_) => {}
                None => continue,
            }
            // JS Map semantics: a repeated class keeps its first slot, latest value wins.
            let slot = match slot_map.entry(p.class_hash) {
                Entry::Vacant(vacant) => {
                    vacant.insert((printed.len() as u32, Vec::new()));
                    None
                }
                Entry::Occupied(mut occupied) => {
                    let same_class =
                        |&slot: &u32| rule(printed[slot as usize]).class_name == p.class_name;
                    let (first, rest) = occupied.get_mut();
                    let slot = std::iter::once(&*first)
                        .chain(rest.iter())
                        .copied()
                        .find(same_class);
                    if slot.is_none() {
                        rest.push(printed.len() as u32);
                    }
                    slot
                }
            };
            match slot {
                Some(slot) => printed[slot as usize] = entry,
                None => {
                    printed.push(entry);
                    let band = bands.last_mut().expect("a band is open");
                    band.end += 1;
                    if !in_layered {
                        band.bare_end += 1;
                    }
                }
            }
        }
        printed
    };

    let header = if use_layers_on {
        layers_header(cfg, bands.len())
    } else {
        String::new()
    };
    let logical = logical_float_block(files.iter().any(|f| f.logical_float));
    let bumps = !use_layers_on && !cfg.legacy_disable_layers;

    // The last render's size is the best estimate of this one's; the first
    // render adds the texts up.
    let body_len = if *rendered_len > 0 {
        *rendered_len + *rendered_len / 16 + 256
    } else {
        let mut bytes = 0;
        for (index, band) in bands.iter().enumerate() {
            let bump = if bumps { 9 * index } else { 0 };
            for &entry in &printed[band.start..band.end] {
                let p = rule(entry);
                bytes += p.ltr.len() + bump + 1;
                bytes += p.rtl.as_ref().map_or(0, |rtl| rtl.len() + bump + 48);
            }
            bytes += 32;
        }
        bytes
    };
    let mut out = String::with_capacity(logical.len() + header.len() + body_len);
    out.push_str(&logical);
    out.push_str(&header);
    for (index, band) in bands.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let mut printed_part = false;
        for (part, wrap) in [
            (band.start..band.bare_end, false),
            (band.bare_end..band.end, true),
        ] {
            if part.is_empty() {
                continue;
            }
            if printed_part {
                out.push('\n');
            }
            printed_part = true;
            if wrap {
                out.push_str("@layer ");
                out.push_str(&layer_name(layer_prefix, index));
                out.push_str("{\n");
            }
            for (j, &entry) in printed[part].iter().enumerate() {
                if j > 0 {
                    out.push('\n');
                }
                let p = rule(entry);
                match p.plain_split {
                    Some(_) if !bumps || index == 0 => out.push_str(&p.ltr),
                    Some(split) => push_split(&mut out, &p.ltr, split as usize, index),
                    None => {
                        let rtl = p.rtl.as_deref().filter(|s| !s.is_empty());
                        push_chunk(&mut out, &p.ltr, rtl, index, cfg);
                    }
                }
            }
            if wrap {
                out.push_str("\n}");
            }
        }
    }
    *rendered_len = out.len();
    out
}

#[cfg(test)]
mod preparation_tests {
    use super::*;

    #[test]
    fn shared_rule_survives_until_its_last_owner_is_removed() {
        let rule = StylexRule {
            class_name: "xshared".into(),
            ltr: ".xshared{color:red}".into(),
            rtl: None,
            const_key: None,
            const_val: None,
            priority: 3000.0,
        };
        let cfg = AssembleConfig::default();
        let mut registry = RuleRegistry::new();
        for rank in 0..64 {
            registry.set_file_rules(&format!("{rank:03}.ts"), vec![rule.clone(); 8]);
        }
        registry.emit(&cfg).unwrap();
        assert!(registry.incr[0].owners.is_some());
        assert_eq!(registry.incr[0].sorted.len(), 1);
        for rank in 0..64 {
            registry.set_file_rules(&format!("{rank:03}.ts"), vec![]);
            let css = registry.emit(&cfg).unwrap();
            assert_eq!(css.is_empty(), rank == 63);
            assert_eq!(Ok(css), assemble(&registry.all_rules(), &cfg));
        }
        assert!(registry.incr[0].owners.as_ref().unwrap().is_empty());
    }

    // Swapping a file that declares no constants must be answered by the
    // splice path, with the 0.19.2 order (re-priced constant wrappers, width
    // bounds) and layer parts (priority 0 bare) intact.
    #[test]
    fn incremental_splice_keeps_repriced_order_and_layer_parts() {
        let rule = |class_name: &str, ltr: &str, priority: f64| StylexRule {
            class_name: class_name.into(),
            ltr: ltr.into(),
            rtl: None,
            const_key: None,
            const_val: None,
            priority,
        };
        let breakpoint = StylexRule {
            class_name: "xbp".into(),
            ltr: "".into(),
            rtl: None,
            const_key: Some("xbp".into()),
            const_val: Some(Box::new(serde_json::json!("@media (min-width: 768px)"))),
            priority: 0.0,
        };
        let configs = [
            AssembleConfig::default(),
            AssembleConfig::from_json(&serde_json::json!({"useLayers": true})).unwrap(),
        ];
        let mut registry = RuleRegistry::new();
        registry.set_file_rules(
            "a.ts",
            vec![
                breakpoint,
                rule("xvars", ":root, .xvars{--xa:red;}", 0.0),
                rule(
                    "xvars-1lveb7",
                    "@media (prefers-color-scheme: dark){:root, .xvars{--xa:white;}}",
                    0.1,
                ),
            ],
        );
        registry.set_file_rules(
            "b.ts",
            vec![
                rule("xa", "var(--xbp){.xa.xa{color:red}}", 6000.0),
                rule(
                    "xb",
                    "@media (min-width: 1024px){.xb.xb{color:red}}",
                    3200.0,
                ),
            ],
        );
        registry.set_file_rules("c.ts", vec![rule("xc", ".xc{color:red}", 3000.0)]);
        for cfg in &configs {
            registry.emit(cfg).unwrap();
        }
        registry.set_file_rules(
            "c.ts",
            vec![
                rule("xkf-B", "@keyframes xkf-B{from{opacity:0;}}", 0.0),
                rule("xd", "var(--xbp){.xd.xd{color:blue}}", 6000.0),
                rule("xe", "@media (min-width: 640px){.xe.xe{color:red}}", 3200.0),
                rule("xf", "@media (max-width: 639px){.xf.xf{color:red}}", 3200.0),
            ],
        );
        let want = [
            ":root, .xvars{--xa:red;}\n@keyframes xkf-B{from{opacity:0;}}\n@media (prefers-color-scheme: dark){:root, .xvars{--xa:white;}}\n@media (min-width: 640px){.xe.xe:not(#\\#){color:red}}\n@media (min-width: 768px){.xd.xd:not(#\\#){color:blue}}\n@media (min-width: 768px){.xa.xa:not(#\\#){color:red}}\n@media (min-width: 1024px){.xb.xb:not(#\\#){color:red}}\n@media (max-width: 639px){.xf.xf:not(#\\#){color:red}}",
            "\n@layer priority1, priority2;\n:root, .xvars{--xa:red;}\n@keyframes xkf-B{from{opacity:0;}}\n@layer priority1{\n@media (prefers-color-scheme: dark){:root, .xvars{--xa:white;}}\n}\n@layer priority2{\n@media (min-width: 640px){.xe.xe{color:red}}\n@media (min-width: 768px){.xd.xd{color:blue}}\n@media (min-width: 768px){.xa.xa{color:red}}\n@media (min-width: 1024px){.xb.xb{color:red}}\n@media (max-width: 639px){.xf.xf{color:red}}\n}",
        ];
        for (cfg, want) in configs.iter().zip(want) {
            let Ok(spliced) = registry.try_incremental(cfg) else {
                panic!("left the splice path");
            };
            assert_eq!(spliced, want);
            assert_eq!(Ok(spliced), assemble(&registry.all_rules(), cfg));
        }
    }

    #[test]
    fn edits_that_remove_repetition_switch_back_to_inline_storage() {
        let rule = |i: usize| StylexRule {
            class_name: format!("x{i}").into(),
            ltr: format!(".x{i}{{width:{i}px}}").into(),
            rtl: None,
            const_key: None,
            const_val: None,
            priority: 3000.0,
        };
        let mut registry = RuleRegistry::new();
        let cfg = AssembleConfig::default();
        for i in 0..64 {
            registry.set_file_rules(&format!("{i:03}.ts"), vec![rule(0); 8]);
        }
        registry.emit(&cfg).unwrap();
        assert!(registry.incr[0].pool.is_some());
        for i in 0..64 {
            registry.set_file_rules(
                &format!("{i:03}.ts"),
                (1 + i * 8..1 + (i + 1) * 8).map(rule).collect(),
            );
            assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
        }
        assert!(registry.incr[0].pool.is_none());
    }

    #[test]
    fn keyed_order_fallback_matches_direct_comparator_with_duplicates() {
        let mk = |class_name: &str, ltr: &str, priority: f64| StylexRule {
            class_name: class_name.into(),
            ltr: ltr.into(),
            rtl: None,
            const_key: None,
            const_val: None,
            priority,
        };
        // Non-ASCII triggers unverified keys and NaN triggers non-finite priority,
        // both alongside duplicate rules so `unique.len() != non_const.len()`.
        let rules = vec![
            mk("xa", ".xa{content:\"café\"}", 3000.0),
            mk("xb", ".xb{color:red}", 3000.0),
            mk("xa", ".xa{content:\"café\"}", 3000.0),
            mk("xc", ".xc{content:\"naïve\"}", f64::NAN),
            mk("xb", ".xb{color:red}", 3000.0),
            mk("xd", ".xd{color:blue}", 3000.0),
        ];
        for legacy in [false, true] {
            let cfg = AssembleConfig {
                use_legacy_classnames_sort: legacy,
                ..AssembleConfig::default()
            };
            assert_eq!(
                assemble(&rules, &cfg),
                assemble_with_comparator(&rules, &cfg, default_locale_cmp),
                "legacy={legacy}"
            );
        }
    }
}

// The optimized paths against the plain forms they replace: the per-constant
// substitution loop, the linear constant lookup, the allocating printer.
#[cfg(test)]
mod parity_tests {
    use super::*;

    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) % n as u64) as usize
        }

        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[self.below(items.len())]
        }
    }

    // Upstream's loop as written: every constant's step over the running text.
    fn substitute_sequential(text: &str, consts: &ConstTable) -> String {
        let mut cur = text.to_string();
        for c in &consts.consts {
            cur = cur.replace(c.var_ref.as_str(), &c.replacement);
            if let Some((needle, target)) = &c.key_rewrite {
                cur = js_replace_all(&cur, needle, target);
            }
        }
        cur
    }

    const NAMES: &[&str] = &[
        "a", "b", "ab", "a-b", "x1", "x_2", "-a", "--a", "t", "", "a.b", "a b", "é", "a)", "$&",
    ];

    #[test]
    fn indexed_substitution_matches_the_sequential_loop() {
        // Unresolved on purpose: values that name later (and earlier)
        // constants exercise the order the steps run in.
        const VALUES: &[&str] = &[
            "red",
            "1px",
            "",
            "var(--a)",
            "var(--b)",
            "var(--t)",
            "var(--x1) var(--b)",
            "var(--a.b)",
            "var(--real, 1px)",
            "var( --t )",
            "var(--$`)",
            "var(--$')",
            "var(--$&)",
            "var(--$$)",
            "var(--t) var(--b)",
            "var(--",
            "b)",
            "--a:1",
            "var(--a b)",
            "@media (min-width: 1px)",
        ];
        const PIECES: &[&str] = &[
            "var(--a)",
            "var(--b)",
            "var(--ab)",
            "var(--a-b)",
            "var(--x1)",
            "var(--x_2)",
            "var(---a)",
            "var(----a)",
            "var(--t)",
            "var(--)",
            "var(--a.b)",
            "var(--a b)",
            "var(--é)",
            "var(--a))",
            "var(--$&)",
            "var(--unknown)",
            "var(--",
            "--a:",
            "--b:1px;",
            "---a:",
            "x--ab:",
            "--x_2:",
            "--a b:",
            "--a.b:",
            "--é:",
            "--:",
            "--t:",
            ")",
            ":",
            "{",
            "}",
            ".x",
            " ",
            "color:",
            "-",
            "(",
        ];
        let mut rng = Lcg(0x5eed_0001);
        let mut rewritten = 0;
        for _ in 0..20_000 {
            let mut decls: Vec<ConstDecl> = Vec::new();
            for _ in 0..rng.below(6) {
                let name = rng.pick(NAMES);
                if decls.iter().all(|(known, _)| &**known != name) {
                    decls.push((name.into(), ConstVal::Str(rng.pick(VALUES).to_string())));
                }
            }
            let table = prepare_consts(&decls);
            let text: String = (0..rng.below(8)).map(|_| rng.pick(PIECES)).collect();
            let got = substitute_consts(&text, &table);
            let want = substitute_sequential(&text, &table);
            rewritten += usize::from(want != text);
            assert_eq!(
                got.as_deref().unwrap_or(&text),
                want,
                "text {text:?} consts {decls:?}"
            );
            // The hand-over to the sequential loop, at every step it can
            // happen on.
            for cap in 0..3 {
                let got = substitute_consts_capped(&text, &table, cap);
                assert_eq!(
                    got.as_deref().unwrap_or(&text),
                    want,
                    "cap {cap} text {text:?} consts {decls:?}"
                );
            }
        }
        assert!(rewritten > 2_000, "too few rewriting cases: {rewritten}");
    }

    // `count` constants with `var()` values, the kind whose step rewrites
    // both its references and its declaration keys. Unresolved on purpose:
    // every third names a later constant, whose step rewrites it again.
    fn var_valued_consts(count: usize) -> ConstTable {
        let decls: Vec<ConstDecl> = (0..count)
            .map(|i| {
                let value = match i % 3 {
                    0 => format!("var(--t{i})"),
                    1 => format!("var( --t{i} , {i}px)"),
                    _ => format!("var(--c{})", (i + 7) % count),
                };
                (format!("c{i}").into(), ConstVal::Str(value))
            })
            .collect();
        let table = prepare_consts(&decls);
        assert!(table.rewrites_keys && table.by_name.len() == count);
        table
    }

    // One rule that all `count` constants rewrite, the last constant first so
    // the search for a step's constant reads past all the later ones.
    // `declaration(i)` is what the rule says about constant `i`.
    fn const_heavy_rule(count: usize, declaration: impl Fn(usize) -> String) -> String {
        let declarations: String = (0..count).rev().map(declaration).collect();
        format!(".x{{{declarations}}}")
    }

    #[test]
    fn a_const_heavy_rule_matches_the_sequential_loop_at_any_cap() {
        for count in [1, 31, 32, 33, 34, 250] {
            let table = var_valued_consts(count);
            // Each constant by key and by reference; then only by reference,
            // under keys no constant owns.
            let texts = [
                const_heavy_rule(count, |i| format!("--c{i}:var(--c{i});")),
                const_heavy_rule(count, |i| format!("--v{i}:var(--c{i}) var(--v{i});")),
            ];
            for text in &texts {
                let want = substitute_sequential(text, &table);
                assert_ne!(want, *text);
                assert_eq!(
                    substitute_consts(text, &table).as_deref(),
                    Some(want.as_str()),
                    "{count} constants"
                );
                for cap in [0, 1, 2, 31, 32, 33, 100, u32::MAX] {
                    assert_eq!(
                        substitute_consts_capped(text, &table, cap).as_deref(),
                        Some(want.as_str()),
                        "{count} constants, cap {cap}"
                    );
                }
            }
        }
    }

    // The rule every constant rewrites a key of, between values that read
    // plain variables: without the cap each of its steps looks up every name
    // in the text again, with it the rule costs what upstream's loop does.
    // Both are timed here, so the comparison holds on a slow or busy machine.
    #[test]
    fn a_const_heavy_rule_is_not_rescanned_for_every_step() {
        let table = var_valued_consts(600);
        let text = const_heavy_rule(600, |i| format!("--c{i}:var(--a) var(--b);"));
        let want = substitute_sequential(&text, &table);
        let best_of = |cap: u32| {
            (0..3)
                .map(|_| {
                    let start = std::time::Instant::now();
                    let got = substitute_consts_capped(&text, &table, cap);
                    let elapsed = start.elapsed();
                    assert_eq!(got.as_deref(), Some(want.as_str()));
                    elapsed
                })
                .min()
                .unwrap()
        };
        let (capped, indexed) = (best_of(MAX_INDEXED_STEPS), best_of(u32::MAX));
        assert!(
            capped * 2 < indexed,
            "capped {capped:?} vs indexed throughout {indexed:?}"
        );
    }

    // The opposite rule: a hundred constants named out of twenty thousand.
    // Its steps are few and the constants between them many, so handing the
    // rest to the sequence after the first steps would search the text for
    // thousands of constants it does not name.
    #[test]
    fn a_rule_naming_few_of_many_constants_stays_indexed() {
        const COUNT: usize = 20_000;
        let table = var_valued_consts(COUNT);
        let spread: String = (0..100)
            .map(|n| format!("--v{n}:var(--c{}) var(--v{n});", n * 200 + 1))
            .collect();
        // Sparse, then dense: forty spread constants, then each of the last
        // two hundred. The first weighing keeps the index, a later one meets
        // the dense tail.
        let tail: String = (0..40)
            .map(|n| n * 475)
            .chain(COUNT - 200..COUNT)
            .map(|i| format!("--c{i}:var(--c{i});"))
            .collect();
        let texts = [format!(".x{{{spread}}}"), format!(".x{{{tail}}}")];
        for text in &texts {
            let want = substitute_sequential(text, &table);
            assert_ne!(want, *text);
            for cap in [0, 1, 2, 3, 31, 32, 33, 64, 100, u32::MAX] {
                assert_eq!(
                    substitute_consts_capped(text, &table, cap).as_deref(),
                    Some(want.as_str()),
                    "cap {cap}"
                );
            }
        }

        let text = &texts[0];
        let best_of = |cap: u32| {
            (0..5)
                .map(|_| {
                    let start = std::time::Instant::now();
                    let got = substitute_consts_capped(text, &table, cap);
                    let elapsed = start.elapsed();
                    assert!(got.is_some());
                    elapsed
                })
                .min()
                .unwrap()
        };
        let (capped, indexed) = (best_of(MAX_INDEXED_STEPS), best_of(u32::MAX));
        assert!(
            capped <= indexed * 2,
            "capped {capped:?} vs indexed throughout {indexed:?}"
        );
    }

    #[test]
    fn live_constants_are_counted_once_from_the_step_on() {
        let decls: Vec<ConstDecl> = [
            ("a", "1px"),
            ("b", "var(--t)"),
            ("a.b", "2px"),
            ("c", "var(--u)"),
            ("d", "3px"),
            ("e f", "4px"),
        ]
        .map(|(name, value)| (name.into(), ConstVal::Str(value.to_string())))
        .into();
        let table = prepare_consts(&decls);
        assert_eq!(table.irregular, [2, 5]);
        // `a` and `c` twice by reference, `b` and `c` by key; `d` owns a key
        // but rewrites none, and `--z` is no constant.
        let text = ".x{--b:var(--a) var(--a);--c:var(--c) var(--c);--d:var(--z)}";
        // From each position on: the named constants at or after it, plus
        // the irregular ones, which are counted unprobed.
        for (from, named) in [(0, 3), (1, 2), (2, 1), (3, 1), (4, 0), (5, 0), (6, 0)] {
            let irregular = table.irregular.iter().filter(|&&i| i >= from).count();
            assert_eq!(
                table.live_count(text, from),
                named + irregular,
                "from {from}"
            );
        }
        assert_eq!(table.live_count(".x{color:red}", 0), 2);
    }

    #[test]
    fn nan_constants_compare_equal_to_themselves() {
        let nan = || ConstVal::from_json(&serde_json::json!({"__jsnum": "NaN"}));
        assert!(matches!(nan(), ConstVal::Num(n) if n.is_nan()));
        assert_eq!(nan(), nan());
        assert_eq!(ConstVal::Num(1.5), ConstVal::Num(1.5));
        assert_ne!(ConstVal::Num(1.5), ConstVal::Num(2.5));
        assert_ne!(ConstVal::Num(1.0), ConstVal::Str("1".to_string()));
        assert_ne!(nan(), ConstVal::Num(f64::INFINITY));
    }

    #[test]
    fn a_file_preparation_holds_no_spare_capacity() {
        let mut rules: Vec<StylexRule> = (0..5)
            .map(|i| StylexRule {
                class_name: format!("x{i}").into(),
                ltr: format!(".x{i}{{order:{i}}}").into(),
                rtl: None,
                const_key: None,
                const_val: None,
                priority: 3000.0,
            })
            .collect();
        for i in 0..3 {
            rules.insert(
                i * 2,
                StylexRule {
                    class_name: format!("k{i}").into(),
                    ltr: "".into(),
                    rtl: None,
                    const_key: Some(format!("k{i}").into()),
                    const_val: Some(Box::new(serde_json::json!("1px"))),
                    priority: 0.0,
                },
            );
        }
        for mut pool in [None, Some(PreparedPool::default())] {
            let prep = prepare_file(&rules, false, &ConstTable::default(), &mut pool);
            let (len, capacity) = match &prep.rules {
                PreparedRules::Inline(rules) => (rules.len(), rules.capacity()),
                PreparedRules::Shared(rules) => (rules.len(), rules.capacity()),
            };
            assert_eq!((len, capacity), (5, 5));
            assert_eq!(prep.const_decls.len(), 3);
            assert_eq!(prep.const_decls.capacity(), 3);
        }
    }

    // The resolver before the name index: every lookup a scan of the list.
    fn resolve_linear(
        value: &ConstVal,
        consts: &[(String, ConstVal)],
        visited: &mut Vec<String>,
    ) -> Result<ConstVal, AssembleError> {
        let ConstVal::Str(s) = value else {
            return Ok(value.clone());
        };
        let mut result = s.clone();
        let mut scan_from = 0usize;
        while let Some((start, end)) = find_var_ref(&result, scan_from) {
            let ref_name = result[start + 4..end - 1].to_string();
            if visited.contains(&ref_name) {
                return Err(AssembleError::CircularConst(ref_name));
            }
            let ref_key = format!("var({ref_name})");
            let Some((_, ref_val)) = consts.iter().find(|(k, _)| *k == ref_key) else {
                scan_from = end;
                continue;
            };
            visited.push(ref_name.clone());
            let replacement = resolve_linear(&ref_val.clone(), consts, visited)?;
            let needle = result[start..end].to_string();
            result = result.replacen(&needle, &replacement.to_js_string(), 1);
            visited.retain(|v| *v != ref_name);
            scan_from = 0;
        }
        Ok(ConstVal::Str(result))
    }

    fn collect_linear(decls: &[ConstDecl]) -> Result<Vec<ConstDecl>, AssembleError> {
        let mut consts: Vec<(String, ConstVal)> = Vec::new();
        for (name, val) in decls {
            let key = format!("var(--{name})");
            match consts.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = val.clone(),
                None => consts.push((key, val.clone())),
            }
        }
        for i in 0..consts.len() {
            let value = consts[i].1.clone();
            consts[i].1 = resolve_linear(&value, &consts, &mut Vec::new())?;
        }
        Ok(consts
            .into_iter()
            .map(|(key, val)| (key[6..key.len() - 1].into(), val))
            .collect())
    }

    #[test]
    fn indexed_resolution_matches_the_linear_lookup() {
        const VALUES: &[&str] = &[
            "red",
            "",
            "var(--a)",
            "var(--b)",
            "var(--ab) var(--b)",
            "calc(var(--x1) * var(--x1))",
            "var(--t)var(--t)",
            "var(--unknown) var(--a-b)",
            "var(--a.b)",
            "var(--)",
            "var(--x_2",
            "var(---a)",
            "$& var(--b) $$",
        ];
        let mut rng = Lcg(0x5eed_0002);
        let (mut resolved, mut cycles) = (0, 0);
        for _ in 0..20_000 {
            let decls: Vec<ConstDecl> = (0..rng.below(7))
                .map(|_| {
                    let value = match rng.below(10) {
                        0 => ConstVal::Num(rng.below(9) as f64 / 2.0),
                        1 => ConstVal::Other(serde_json::json!([1, null, "x"])),
                        _ => ConstVal::Str(rng.pick(VALUES).to_string()),
                    };
                    (rng.pick(NAMES).into(), value)
                })
                .collect();
            let got = collect_resolved_consts(decls.iter().cloned());
            let want = collect_linear(&decls);
            match &want {
                Ok(_) => resolved += 1,
                Err(_) => cycles += 1,
            }
            assert_eq!(got, want, "consts {decls:?}");
        }
        assert!(resolved > 2_000 && cycles > 2_000, "{resolved} / {cycles}");
    }

    // The printer before it wrote in place: one String per step.
    fn rule_chunk_allocating(
        ltr: &str,
        rtl: Option<&str>,
        index: usize,
        cfg: &AssembleConfig,
    ) -> String {
        fn add_specificity_level(selector: &str, index: usize) -> String {
            let mut out = String::new();
            push_specificity_level(&mut out, selector, index);
            out
        }
        fn add_ancestor_selector(selector: &str, ancestor_selector: &str) -> String {
            if is_selector_exempt(selector) {
                return selector.to_string();
            }
            if !selector.starts_with('@') {
                return format!("{ancestor_selector} {selector}");
            }
            let last_at_rule = selector.rfind('@').unwrap_or(0);
            match selector[last_at_rule..].find('{') {
                Some(off) => {
                    let bracket = last_at_rule + off;
                    format!(
                        "{}{} {}",
                        &selector[..bracket + 1],
                        ancestor_selector,
                        &selector[bracket + 1..]
                    )
                }
                None => format!("{ancestor_selector} {selector}"),
            }
        }
        let use_layers_on = matches!(cfg.use_layers, LayersConfig::On { .. });
        let mut ltr = ltr.to_string();
        let mut rtl = rtl.map(str::to_string);
        if !use_layers_on && !cfg.legacy_disable_layers {
            ltr = add_specificity_level(&ltr, index);
            rtl = rtl.map(|r| add_specificity_level(&r, index));
        }
        ltr = double_theme_classes(&ltr);
        rtl = rtl.map(|r| double_theme_classes(&r));
        match rtl {
            Some(rtl) if cfg.enable_ltr_rtl_comments => {
                format!("/* @ltr begin */{ltr}/* @ltr end */\n/* @rtl begin */{rtl}/* @rtl end */")
            }
            Some(rtl) => format!(
                "{}\n{}",
                add_ancestor_selector(&ltr, "html:not([dir='rtl'])"),
                add_ancestor_selector(&rtl, "html[dir='rtl']")
            ),
            None => ltr,
        }
    }

    #[test]
    fn in_place_printer_matches_the_allocating_one() {
        const TEXTS: &[&str] = &[
            ".a{color:red}",
            ".a::before{color:red}",
            ".a:hover::after{color:red}",
            "@media (min-width: 1px){.a.a{color:red}}",
            "@media (min-width: 1px){@supports (x: y){.a.a.a::after{color:red}}}",
            "@keyframes k-B{from{opacity:0;}to{opacity:1;}}",
            "@position-try --p{top:0;left:0;}",
            ".a, .b:root{--x:1;}",
            ":root, .a{--x:1;}",
            ".thm1, .thm2:root::before{--x:1;}",
            "@media (prefers-color-scheme: dark){.a, .a:root{--x:1;}}",
            "@media (x){:root, .a{--x:1;}}",
            ".a, .b:root, .c, .d:root{}",
            ".a, .b:root",
            "@media x",
            "@a{@b",
            "@a @b{.x{y:z}}",
            "@nobrace",
            ".é, .b:root{content:\"é\"}",
            "é",
            "x",
            "",
        ];
        let configs = [
            serde_json::json!({}),
            serde_json::json!({"useLayers": true}),
            serde_json::json!({"legacyDisableLayers": true}),
            serde_json::json!({"enableLTRRTLComments": true}),
            serde_json::json!({"useLayers": true, "enableLTRRTLComments": true}),
        ]
        .map(|raw| AssembleConfig::from_json(&raw).unwrap());
        for cfg in &configs {
            for index in 0..4 {
                for ltr in TEXTS {
                    let rtls = TEXTS
                        .iter()
                        .filter(|rtl| !rtl.is_empty())
                        .map(|rtl| Some(*rtl));
                    for rtl in std::iter::once(None).chain(rtls) {
                        let mut got = String::from("before\n");
                        push_chunk(&mut got, ltr, rtl, index, cfg);
                        let want = rule_chunk_allocating(ltr, rtl, index, cfg);
                        assert_eq!(got, format!("before\n{want}"), "{ltr:?} {rtl:?} {index}");
                    }
                }
            }
        }
    }

    #[test]
    fn priority_bits_order_like_the_priority_comparison() {
        let priorities = [
            -1e308, -3000.5, -1.0, -5e-324, -0.0, 0.0, 5e-324, 0.1, 1.0, 2999.0, 3000.0, 1e308,
        ];
        for a in priorities {
            for b in priorities {
                assert_eq!(
                    priority_bits(a).cmp(&priority_bits(b)),
                    compare_priorities(a, b).unwrap_or(Ordering::Equal),
                    "{a} vs {b}"
                );
            }
        }
    }

    #[test]
    fn prepared_rule_stays_compact() {
        assert!(std::mem::size_of::<RuleKey>() <= 40);
        assert!(std::mem::size_of::<PreparedRule>() <= 120);
    }
}

#[cfg(test)]
mod splice_tests {
    use super::*;

    fn rule(class_name: &str, ltr: &str, priority: f64) -> StylexRule {
        StylexRule {
            class_name: class_name.into(),
            ltr: ltr.into(),
            rtl: None,
            const_key: None,
            const_val: None,
            priority,
        }
    }

    fn constant(name: &str, value: serde_json::Value) -> StylexRule {
        StylexRule {
            const_key: Some(name.into()),
            const_val: Some(Box::new(value)),
            ..rule(name, "", 0.0)
        }
    }

    fn configs() -> [AssembleConfig; 3] {
        [
            serde_json::json!({}),
            serde_json::json!({"useLayers": true}),
            serde_json::json!({"useLegacyClassnamesSort": true}),
        ]
        .map(|raw| AssembleConfig::from_json(&raw).unwrap())
    }

    // The splice path must answer, and with the one-shot answer.
    fn assert_spliced(registry: &mut RuleRegistry, what: &str) {
        for cfg in &configs() {
            let spliced = registry.try_incremental(cfg);
            assert!(spliced.is_ok(), "{what}: left the splice path");
            assert_eq!(
                spliced.map_err(|_| ()),
                assemble(&registry.all_rules(), cfg).map_err(|_| ())
            );
        }
    }

    // A small project: per-file rules of its own, some shared atoms, a media
    // rule, a priority-0 rule, and an rtl pair.
    fn file_rules(i: usize) -> Vec<StylexRule> {
        vec![
            rule(
                &format!("xown{i}"),
                &format!(".xown{i}{{width:{i}px}}"),
                4000.0,
            ),
            rule("xflex", ".xflex{display:flex}", 3000.0),
            rule(
                &format!("xmq{}", i % 3),
                &format!(
                    "@media (min-width: {}px){{.xmq{0}.xmq{0}{{color:red}}}}",
                    i % 3
                ),
                3200.0,
            ),
            rule(
                &format!("xvars{i}"),
                &format!(":root, .xvars{i}{{--v{i}:red;}}"),
                0.0,
            ),
            StylexRule {
                rtl: Some(format!(".xdir{}{{margin-right:0}}", i % 2).into()),
                ..rule(
                    &format!("xdir{}", i % 2),
                    &format!(".xdir{}{{margin-left:0}}", i % 2),
                    3000.0,
                )
            },
            rule(
                &format!("xpad{}", i % 4),
                &format!(".xpad{}{{padding:0}}", i % 4),
                1000.0,
            ),
        ]
    }

    #[test]
    fn added_and_removed_files_are_spliced_in_place() {
        let mut registry = RuleRegistry::new();
        for i in [10, 30, 50] {
            registry.set_file_rules(&format!("f{i}.ts"), file_rules(i));
        }
        for cfg in &configs() {
            registry.emit(cfg).unwrap();
        }
        // In the middle, at the end, at the front.
        for i in [20, 60, 0] {
            registry.set_file_rules(&format!("f{i:02}.ts"), file_rules(i));
            assert_spliced(&mut registry, "add");
        }
        registry.remove_file("f30.ts");
        assert_spliced(&mut registry, "remove");
        // One emit sees an add, an edit and two removals; a file added and
        // removed in between is never seen at all.
        registry.set_file_rules("f40.ts", file_rules(40));
        registry.set_file_rules("f10.ts", file_rules(11));
        registry.remove_file("f00.ts");
        registry.remove_file("f60.ts");
        registry.set_file_rules("f99.ts", file_rules(99));
        registry.remove_file("f99.ts");
        assert_spliced(&mut registry, "mixed");
        // A removed file comes back.
        registry.set_file_rules("f30.ts", file_rules(31));
        assert_spliced(&mut registry, "re-add");
        for path in ["f10.ts", "f20.ts", "f30.ts", "f40.ts", "f50.ts"] {
            registry.remove_file(path);
            assert_spliced(&mut registry, "drain");
        }
        assert_eq!(registry.emit(&configs()[0]), Ok(String::new()));
        registry.set_file_rules("f70.ts", file_rules(70));
        assert_spliced(&mut registry, "refill");
        assert!(registry.incr.iter().all(|state| state.paths == ["f70.ts"]));
    }

    #[test]
    fn added_and_removed_files_are_spliced_among_shared_owners() {
        let mut registry = RuleRegistry::new();
        // Heavy repetition: every file repeats the same few atoms.
        let atoms = |i: usize| -> Vec<StylexRule> {
            (0..8)
                .map(|j| {
                    let n = (i + j) % 12;
                    rule(
                        &format!("xatom{n}"),
                        &format!(".xatom{n}{{order:{n}}}"),
                        3000.0,
                    )
                })
                .collect()
        };
        for i in 0..64 {
            registry.set_file_rules(&format!("f{:03}.ts", i * 2), atoms(i));
        }
        for cfg in &configs() {
            registry.emit(cfg).unwrap();
        }
        assert!(registry.incr.iter().all(|state| state.owners.is_some()));
        for i in 0..40 {
            let odd = format!("f{:03}.ts", i * 2 + 1);
            let mut rules = atoms(i + 3);
            // A class only this file owns: it leaves with the file.
            rules.push(rule(
                &format!("xonly{i}"),
                &format!(".xonly{i}{{top:0}}"),
                4000.0,
            ));
            registry.set_file_rules(&odd, rules);
            assert_spliced(&mut registry, "add");
            registry.remove_file(&format!("f{:03}.ts", i * 2));
            assert_spliced(&mut registry, "remove");
            if i % 3 == 0 {
                registry.remove_file(&odd);
                assert_spliced(&mut registry, "remove the added");
            }
        }
        assert!(registry.incr.iter().all(|state| state.owners.is_some()));
        // Every sort entry is still the first owner of its class.
        for state in &registry.incr {
            let owners = state.owners.as_ref().unwrap();
            assert_eq!(state.sorted.len(), owners.len());
            let pool = state.pool.as_ref().unwrap();
            assert_eq!(
                pool.live,
                pool.buckets.values().map(Vec::len).sum::<usize>()
            );
            assert_eq!(pool.live, owners.len());
        }
    }

    #[test]
    fn untouched_constants_keep_the_splice_and_changed_ones_rebuild() {
        let tokens = |px: u32| {
            vec![
                constant("xbp", serde_json::json!("@media (min-width: 768px)")),
                constant("xgap", serde_json::json!(format!("{px}px"))),
                rule("xvars", ":root, .xvars{--xa:red;}", 0.0),
            ]
        };
        let uses = vec![
            rule("xa", "var(--xbp){.xa.xa{color:red}}", 6000.0),
            rule("xb", ".xb{gap:var(--xgap)}", 2000.0),
        ];
        let mut registry = RuleRegistry::new();
        registry.set_file_rules("tokens.ts", tokens(4));
        registry.set_file_rules("uses.ts", uses.clone());
        for cfg in &configs() {
            registry.emit(cfg).unwrap();
        }
        // The token file changes around its constants.
        let mut edited = tokens(4);
        edited.push(rule("xvars2", ":root, .xvars2{--xb:blue;}", 0.0));
        registry.set_file_rules("tokens.ts", edited);
        assert_spliced(&mut registry, "token file edit");
        // A value changes: the prepared texts are stale.
        registry.set_file_rules("tokens.ts", tokens(8));
        for cfg in &configs() {
            assert!(registry.try_incremental(cfg).is_err());
            let css = registry.emit(cfg).unwrap();
            assert!(css.contains("gap:8px"), "{css}");
            assert_eq!(Ok(css), assemble(&registry.all_rules(), cfg));
        }
        // An added file redefines a constant (last wins), then leaves again.
        registry.set_file_rules(
            "z-override.ts",
            vec![constant("xgap", serde_json::json!("1em"))],
        );
        for cfg in &configs() {
            assert!(registry.try_incremental(cfg).is_err());
            assert!(registry.emit(cfg).unwrap().contains("gap:1em"));
        }
        registry.remove_file("z-override.ts");
        for cfg in &configs() {
            assert!(registry.try_incremental(cfg).is_err());
            assert!(registry.emit(cfg).unwrap().contains("gap:8px"));
        }
        // A file that declares no constants comes and goes on the splice path.
        registry.set_file_rules("more.ts", uses);
        assert_spliced(&mut registry, "add beside constants");
        registry.remove_file("more.ts");
        assert_spliced(&mut registry, "remove beside constants");
    }

    #[test]
    fn a_blocked_config_is_retried_only_when_its_blocker_changes() {
        let cfg = AssembleConfig::default();
        let unsafe_rule = rule("xu", ".xu{color:red}", f64::NAN);
        let mut registry = RuleRegistry::new();
        registry.set_file_rules("a.ts", file_rules(1));
        registry.set_file_rules("b.ts", vec![unsafe_rule.clone()]);
        registry.set_file_rules("c.ts", vec![unsafe_rule]);
        let blocked_on = |registry: &RuleRegistry| -> Vec<String> {
            registry
                .blocked
                .iter()
                .map(|blocked| match &blocked.on {
                    Blocker::File { path, .. } => path.clone(),
                    Blocker::Consts => "<consts>".to_string(),
                })
                .collect()
        };
        assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
        assert_eq!(blocked_on(&registry), ["b.ts"]);
        assert!(registry.incr.is_empty());

        // Other files change: the config stays blocked, nothing is rebuilt.
        registry.set_file_rules("a.ts", file_rules(2));
        registry.set_file_rules("d.ts", file_rules(3));
        registry.remove_file("d.ts");
        assert_eq!(blocked_on(&registry), ["b.ts"]);
        assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
        assert!(registry.incr.is_empty());

        // The blocker is fixed: the retry finds the next one.
        registry.set_file_rules("b.ts", file_rules(4));
        assert!(registry.blocked.is_empty());
        assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
        assert_eq!(blocked_on(&registry), ["c.ts"]);

        // A constant changes: it reaches every file's rules, so retry.
        registry.set_file_rules("e.ts", vec![constant("xk", serde_json::json!("1px"))]);
        assert!(registry.blocked.is_empty());
        assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
        assert_eq!(blocked_on(&registry), ["c.ts"]);

        // The last blocker leaves: the splice path takes over.
        registry.remove_file("c.ts");
        assert!(registry.blocked.is_empty());
        assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
        assert!(registry.blocked.is_empty());
        assert_eq!(registry.incr.len(), 1);
        registry.set_file_rules("a.ts", file_rules(5));
        assert!(registry.try_incremental(&cfg).is_ok());
    }

    // The blocker, a rule of non-finite priority, reads a constant: an edit
    // to it is judged against the constants it was blocked on.
    #[test]
    fn an_edited_blocker_is_judged_alone_and_unblocks_once_safe() {
        let cfg = AssembleConfig::default();
        let glyph_user = |n: usize| {
            rule(
                &format!("xu{n}"),
                &format!(".xu{n}{{content:var(--xglyph)}}"),
                f64::INFINITY,
            )
        };
        let mut registry = RuleRegistry::new();
        registry.set_file_rules("a.ts", file_rules(1));
        registry.set_file_rules("b.ts", vec![glyph_user(0)]);
        registry.set_file_rules("c.ts", file_rules(2));
        registry.set_file_rules(
            "tokens.ts",
            vec![constant("xglyph", serde_json::json!("\"\u{436}\""))],
        );
        // The constants the one blocked entry was prepared against.
        let blocked_consts = |registry: &RuleRegistry| -> Arc<ConstTable> {
            match &registry.blocked[..] {
                [
                    Blocked {
                        on: Blocker::File { path, consts },
                        ..
                    },
                ] => {
                    assert_eq!(path, "b.ts");
                    Arc::clone(consts)
                }
                _ => panic!("expected one config blocked on a file"),
            }
        };
        assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
        let consts = blocked_consts(&registry);
        assert!(registry.incr.is_empty());

        // Edited but still unsafe: the entry survives the edit and the emit
        // after it. A retry would have built its constants anew.
        for n in 1..4 {
            let mut rules = file_rules(n + 2);
            rules.push(glyph_user(n));
            registry.set_file_rules("b.ts", rules);
            assert!(Arc::ptr_eq(&consts, &blocked_consts(&registry)));
            let css = registry.emit(&cfg);
            assert!(css.as_ref().unwrap().contains("content:\"\u{436}\""));
            assert_eq!(css, assemble(&registry.all_rules(), &cfg));
            assert!(Arc::ptr_eq(&consts, &blocked_consts(&registry)));
            assert!(registry.incr.is_empty());
        }

        // Edited to be safe: unblocked at once, and the next emit builds the
        // state that later edits are spliced into.
        registry.set_file_rules("b.ts", file_rules(6));
        assert!(registry.blocked.is_empty());
        assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
        assert!(registry.blocked.is_empty());
        assert_eq!(registry.incr.len(), 1);
        registry.set_file_rules("b.ts", file_rules(7));
        assert!(registry.try_incremental(&cfg).is_ok());
        assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));

        // Unsafe again, then removed: a blocker that is gone blocks nothing.
        registry.set_file_rules("b.ts", vec![glyph_user(8)]);
        assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
        assert!(!Arc::ptr_eq(&consts, &blocked_consts(&registry)));
        registry.remove_file("b.ts");
        assert!(registry.blocked.is_empty());
        assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
        assert_eq!(registry.incr.len(), 1);
    }

    #[test]
    fn a_nan_constant_keeps_its_file_on_the_splice_path() {
        let tokens = |extra: usize| {
            let mut rules = vec![
                constant("xnan", serde_json::json!({"__jsnum": "NaN"})),
                constant("xgap", serde_json::json!(4)),
                rule("xn", ".xn{order:var(--xnan)}", 3000.0),
            ];
            rules.extend((0..extra).map(|i| {
                rule(
                    &format!("xextra{i}"),
                    &format!(".xextra{i}{{top:{i}px}}"),
                    3000.0,
                )
            }));
            rules
        };
        let mut registry = RuleRegistry::new();
        registry.set_file_rules("tokens.ts", tokens(0));
        registry.set_file_rules("a.ts", file_rules(1));
        for cfg in &configs() {
            assert!(registry.emit(cfg).unwrap().contains("order:NaN"));
        }
        for extra in 1..4 {
            registry.set_file_rules("tokens.ts", tokens(extra));
            assert_spliced(&mut registry, "edit beside a NaN constant");
        }
    }

    #[test]
    fn a_state_no_emit_asks_for_is_dropped_once_its_backlog_outgrows_the_registry() {
        let [active, idle, _] = configs();
        let mut registry = RuleRegistry::new();
        for i in 0..4 {
            registry.set_file_rules(&format!("f{i}.ts"), file_rules(i));
        }
        registry.emit(&active).unwrap();
        registry.emit(&idle).unwrap();
        assert_eq!(registry.incr.len(), 2);

        // Files come and go; only one config is ever emitted.
        let mut dropped_at = None;
        for step in 0..4 * MAX_IDLE_DIRTY {
            let path = format!("tmp{step}.ts");
            registry.set_file_rules(&path, file_rules(step));
            assert!(registry.try_incremental(&active).is_ok());
            registry.remove_file(&path);
            assert!(registry.try_incremental(&active).is_ok());
            let backlog = registry.file_count().max(MAX_IDLE_DIRTY);
            assert!(registry.incr.iter().all(|s| s.dirty.len() <= backlog));
            if registry.incr.len() == 1 {
                dropped_at.get_or_insert(step);
            }
        }
        assert_eq!(dropped_at, Some(MAX_IDLE_DIRTY));
        assert!(registry.incr.iter().all(|state| state.cfg == active));

        // The dropped config is rebuilt when it is asked for again.
        assert_eq!(registry.emit(&idle), assemble(&registry.all_rules(), &idle));
        assert_eq!(registry.incr.len(), 2);
        registry.set_file_rules("f0.ts", file_rules(9));
        for cfg in [&active, &idle] {
            let Ok(spliced) = registry.try_incremental(cfg) else {
                panic!("left the splice path");
            };
            assert_eq!(Ok(spliced), assemble(&registry.all_rules(), cfg));
        }
    }

    // Most of the registry leaves between two emits. The backlog outgrows
    // what is left of the registry, but not the states that held the files:
    // they splice the removals, and are dropped only by a backlog larger than
    // themselves.
    #[test]
    fn a_batch_removing_most_files_is_spliced_by_the_states_that_held_them() {
        let shrunk = || {
            let mut registry = RuleRegistry::new();
            for i in 0..40 {
                registry.set_file_rules(&format!("f{i:02}.ts"), file_rules(i));
            }
            for cfg in &configs() {
                registry.emit(cfg).unwrap();
            }
            for i in (0..40).filter(|i| i % 4 != 0) {
                registry.remove_file(&format!("f{i:02}.ts"));
            }
            assert_eq!(registry.file_count(), 10);
            assert!(registry.file_count().max(MAX_IDLE_DIRTY) < 30);
            assert_eq!(registry.incr.len(), configs().len());
            for state in &registry.incr {
                assert_eq!((state.dirty.len(), state.paths.len()), (30, 40));
            }
            registry
        };

        let mut registry = shrunk();
        assert_spliced(&mut registry, "remove most files");
        for state in &registry.incr {
            assert_eq!((state.dirty.len(), state.paths.len()), (0, 10));
        }

        // No emit comes: the states go once more files were touched than
        // they hold.
        let mut registry = shrunk();
        for step in 0..10 {
            registry.set_file_rules(&format!("tmp{step}.ts"), file_rules(step));
            assert_eq!(registry.incr.len(), configs().len(), "step {step}");
        }
        registry.set_file_rules("tmp10.ts", file_rules(10));
        assert!(registry.incr.is_empty());
        for cfg in &configs() {
            assert_eq!(registry.emit(cfg), assemble(&registry.all_rules(), cfg));
        }
    }

    // A file edited from safe to unsafe is met by the splice that prepares
    // it. The config is blocked on it there, against the constants of the
    // state being spliced: a rebuild would prepare every file against a table
    // of its own to stop at the same one.
    #[test]
    fn a_splice_that_meets_an_unsafe_file_blocks_on_it_without_a_rebuild() {
        let cfg = AssembleConfig::default();
        let unsafe_rule = rule("xu", ".xu{color:red}", f64::INFINITY);
        let glyph_user = rule("xg", ".xg{content:var(--xglyph)}", f64::INFINITY);
        let glyph = |value: &str| constant("xglyph", serde_json::json!(value));
        let with = |i: usize, extra: &StylexRule| {
            let mut rules = file_rules(i);
            rules.push(extra.clone());
            rules
        };
        let seeded = || {
            let mut registry = RuleRegistry::new();
            for (i, path) in ["a.ts", "b.ts", "c.ts"].into_iter().enumerate() {
                registry.set_file_rules(path, file_rules(i));
            }
            registry.set_file_rules("tokens.ts", vec![glyph("\"\u{436}\"")]);
            assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
            assert!(registry.blocked.is_empty());
            let table = Arc::clone(&registry.incr[0].table);
            (registry, table)
        };
        // What the one config is blocked on.
        let blocker = |registry: &RuleRegistry| -> (String, Arc<ConstTable>) {
            assert!(registry.incr.is_empty());
            match &registry.blocked[..] {
                [
                    Blocked {
                        on: Blocker::File { path, consts },
                        ..
                    },
                ] => (path.clone(), Arc::clone(consts)),
                _ => panic!("expected one config blocked on a file"),
            }
        };

        // Edited, added and removed files in one batch, two of them unsafe:
        // the first in path order is the blocker, and the table is the
        // state's own, so no state was built to find it.
        let (mut registry, table) = seeded();
        registry.set_file_rules("a.ts", file_rules(4));
        registry.set_file_rules("b.ts", with(1, &glyph_user));
        registry.set_file_rules("c.ts", vec![unsafe_rule.clone()]);
        registry.set_file_rules("d.ts", file_rules(5));
        registry.remove_file("d.ts");
        let css = registry.emit(&cfg);
        assert!(css.as_ref().unwrap().contains("content:\"\u{436}\""));
        assert_eq!(css, assemble(&registry.all_rules(), &cfg));
        let (path, consts) = blocker(&registry);
        assert_eq!(path, "b.ts");
        assert!(Arc::ptr_eq(&consts, &table));
        // The entry is one `unblock` reads like any other: an edit that
        // leaves the file unsafe keeps it, a safe one drops it, and the
        // retry stops at the next unsafe file.
        registry.set_file_rules("b.ts", with(2, &glyph_user));
        assert!(Arc::ptr_eq(&blocker(&registry).1, &table));
        registry.set_file_rules("b.ts", file_rules(1));
        assert!(registry.blocked.is_empty());
        assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
        let (path, consts) = blocker(&registry);
        assert_eq!(path, "c.ts");
        assert!(!Arc::ptr_eq(&consts, &table));

        // An added file is judged the same way.
        let (mut registry, table) = seeded();
        registry.set_file_rules("b2.ts", vec![unsafe_rule.clone()]);
        assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
        let (path, consts) = blocker(&registry);
        assert_eq!(path, "b2.ts");
        assert!(Arc::ptr_eq(&consts, &table));

        // The same batch changes the constant the unsafe file reads, in a
        // file the splice has not looked at yet: the state's table is stale,
        // and the file blocks against the table the rebuild prepared.
        let (mut registry, table) = seeded();
        registry.set_file_rules("b.ts", with(1, &glyph_user));
        registry.set_file_rules("tokens.ts", vec![glyph("none")]);
        let css = registry.emit(&cfg);
        assert!(css.as_ref().unwrap().contains("content:none"));
        assert_eq!(css, assemble(&registry.all_rules(), &cfg));
        let (path, consts) = blocker(&registry);
        assert_eq!(path, "b.ts");
        assert!(!Arc::ptr_eq(&consts, &table));

        // Constants added around the unsafe file: it blocks, against the
        // table the rebuild prepared. In a file before the unsafe one, after
        // it, and in the unsafe file itself.
        for tokens in ["a-tokens.ts", "z-tokens.ts", "b.ts"] {
            let (mut registry, table) = seeded();
            let mut rules = with(1, &unsafe_rule);
            if tokens == "b.ts" {
                rules.push(constant("xgap", serde_json::json!("4px")));
            } else {
                registry.set_file_rules(tokens, vec![constant("xgap", serde_json::json!("4px"))]);
            }
            registry.set_file_rules("b.ts", rules);
            assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
            let (path, consts) = blocker(&registry);
            assert_eq!(path, "b.ts", "constants in {tokens}");
            assert!(!Arc::ptr_eq(&consts, &table), "constants in {tokens}");
            assert_eq!(consts.consts.len(), 2);
        }
    }

    #[test]
    fn a_constant_cycle_blocks_until_the_constants_change() {
        let cfg = AssembleConfig::default();
        let mut registry = RuleRegistry::new();
        registry.set_file_rules(
            "tokens.ts",
            vec![
                constant("xa", serde_json::json!("var(--xb)")),
                constant("xb", serde_json::json!("var(--xa)")),
            ],
        );
        registry.set_file_rules("a.ts", file_rules(1));
        let cycle = assemble(&registry.all_rules(), &cfg);
        assert!(matches!(cycle, Err(AssembleError::CircularConst(_))));
        assert_eq!(registry.emit(&cfg), cycle);
        assert!(matches!(registry.blocked[0].on, Blocker::Consts));
        registry.set_file_rules("a.ts", file_rules(2));
        assert_eq!(registry.blocked.len(), 1);
        assert_eq!(registry.emit(&cfg), cycle);
        registry.set_file_rules("tokens.ts", vec![constant("xa", serde_json::json!("1px"))]);
        assert!(registry.blocked.is_empty());
        assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
        assert_eq!(registry.incr.len(), 1);
    }

    #[test]
    fn files_added_with_repetition_move_inline_storage_to_shared() {
        let cfg = AssembleConfig::default();
        let mut registry = RuleRegistry::new();
        registry.set_file_rules("a.ts", file_rules(1));
        registry.emit(&cfg).unwrap();
        assert!(registry.incr[0].pool.is_none());
        let atoms: Vec<StylexRule> = (0..8)
            .map(|n| {
                rule(
                    &format!("xatom{n}"),
                    &format!(".xatom{n}{{order:{n}}}"),
                    3000.0,
                )
            })
            .collect();
        for i in 0..64 {
            registry.set_file_rules(&format!("f{i:03}.ts"), atoms.clone());
            assert_eq!(registry.emit(&cfg), assemble(&registry.all_rules(), &cfg));
        }
        assert!(registry.incr[0].pool.is_some());
        assert!(registry.incr[0].owners.is_some());
    }

    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) % n as u64) as usize
        }
    }

    // Random adds, edits and removals over rule pools with little and with
    // heavy repetition (inline and shared storage), with constants, class
    // conflicts, negative priorities and rtl pairs in the mix.
    #[test]
    fn random_file_churn_matches_one_shot() {
        let universe = |classes: usize| -> Vec<StylexRule> {
            let mut rules = Vec::new();
            for n in 0..classes {
                let class = format!("x{n}");
                rules.push(match n % 9 {
                    0 => rule(
                        &class,
                        &format!("@media (min-width: {n}px){{.{class}.{class}{{color:red}}}}"),
                        3200.0,
                    ),
                    1 => rule(&class, &format!(":root, .{class}{{--{class}:red;}}"), 0.0),
                    2 => StylexRule {
                        rtl: Some(format!(".{class}{{right:0}}").into()),
                        ..rule(&class, &format!(".{class}{{left:0}}"), 4000.0)
                    },
                    3 => rule(
                        &class,
                        &format!("var(--xbp){{.{class}.{class}{{top:0}}}}"),
                        6000.0,
                    ),
                    4 => rule(&class, &format!(".{class}{{gap:var(--xgap)}}"), 2000.0),
                    5 => rule(&class, &format!(".{class}{{order:{n}}}"), -1.0),
                    6 => rule(&class, &format!(".{class}, .{class}:root{{--t:1;}}"), 0.5),
                    _ => rule(&class, &format!(".{class}{{width:{n}px}}"), 3000.0),
                });
            }
            // Two texts for one class: the first-slot / last-value case.
            rules.push(rule("x7", ".x7{width:auto}", 3000.0));
            rules.push(constant(
                "xbp",
                serde_json::json!("@media (min-width: 768px)"),
            ));
            rules.push(constant("xgap", serde_json::json!("4px")));
            rules.push(constant("xgap", serde_json::json!("8px")));
            rules
        };
        for (seed, classes, files, per_file) in [
            (1, 40, 48, 12),
            (2, 2000, 24, 16),
            (3, 12, 64, 8),
            (4, 300, 40, 20),
        ] {
            let universe = universe(classes);
            // The tail of the universe (conflict, constants) is drawn rarely.
            let plain = universe.len() - 4;
            let mut rng = Lcg(seed);
            let draw = |rng: &mut Lcg| -> Vec<StylexRule> {
                (0..rng.below(per_file + 1))
                    .map(|_| {
                        let at = match rng.below(60) {
                            0 => plain + rng.below(4),
                            _ => rng.below(plain),
                        };
                        universe[at].clone()
                    })
                    .collect()
            };
            let mut registry = RuleRegistry::new();
            for i in 0..files / 2 {
                registry.set_file_rules(&format!("src/f{:03}.ts", i * 2), draw(&mut rng));
            }
            let configs = configs();
            for step in 0..400 {
                for _ in 0..1 + rng.below(3) {
                    let path = format!("src/f{:03}.ts", rng.below(files));
                    if rng.below(3) == 0 {
                        registry.remove_file(&path);
                    } else {
                        registry.set_file_rules(&path, draw(&mut rng));
                    }
                }
                let cfg = &configs[step % configs.len()];
                assert_eq!(
                    registry.emit(cfg),
                    assemble(&registry.all_rules(), cfg),
                    "seed {seed} step {step}"
                );
            }
        }
    }
}
