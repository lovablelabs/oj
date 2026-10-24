// parity: stylex-0.19.0 packages/@stylexjs/babel-plugin/src/index.js processStylexRules

use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{BuildHasher, BuildHasherDefault};
use std::sync::{Arc, Weak};

use crate::fxhash::{FxHashMap, FxHashSet};
use crate::jsrt::{ASCII_LOCALE_KEYS, locale_key, utf16_cmp};
use crate::rules::StylexRule;
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
#[derive(Clone, Debug, PartialEq)]
enum ConstVal {
    Str(String),
    Num(f64),
    Other(serde_json::Value),
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
    assemble_impl(rules, cfg, None)
}

pub fn assemble_with_comparator(
    rules: &[StylexRule],
    cfg: &AssembleConfig,
    cmp: Comparator,
) -> Result<String, AssembleError> {
    assemble_impl(rules, cfg, Some(cmp))
}

fn is_const_rule(r: &StylexRule) -> bool {
    r.const_key.is_some() && matches!(&r.const_val, Some(v) if !v.is_null())
}

fn const_decl(r: &StylexRule) -> (String, ConstVal) {
    (
        format!("var(--{})", r.class_name),
        ConstVal::from_json(r.const_val.as_ref().unwrap()),
    )
}

// Insertion-ordered "var(--keyhash)" → value map; duplicate keys keep their
// slot, value last-wins (JS Map.set). Each value is then resolved in place.
fn collect_resolved_consts(
    decls: impl Iterator<Item = (String, ConstVal)>,
) -> Result<Vec<(String, ConstVal)>, AssembleError> {
    let mut consts: Vec<(String, ConstVal)> = Vec::new();
    for (key, val) in decls {
        match consts.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = val,
            None => consts.push((key, val)),
        }
    }
    for i in 0..consts.len() {
        let value = consts[i].1.clone();
        let resolved = resolve_constant(&value, &consts, &mut Vec::new())?;
        consts[i].1 = resolved;
    }
    Ok(consts)
}

fn rule_has_logical_float(r: &StylexRule) -> bool {
    fn has(text: &str) -> bool {
        text.contains("--stylex-logical-")
            && (text.contains(LOGICAL_FLOAT_START_VAR) || text.contains(LOGICAL_FLOAT_END_VAR))
    }
    has(&r.ltr) || r.rtl.as_deref().is_some_and(has)
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
// same '\n' the group join would use.
fn rule_chunk(ltr: &str, rtl: Option<&str>, index: usize, cfg: &AssembleConfig) -> String {
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

// The common shape (no rtl, no theme pair) is pushed as slices; the rest takes
// rule_chunk. `", ."` is a literal every theme-pair match contains.
fn push_chunk(out: &mut String, ltr: &str, rtl: Option<&str>, index: usize, cfg: &AssembleConfig) {
    if rtl.is_some() || ltr.contains(", .") {
        out.push_str(&rule_chunk(ltr, rtl, index, cfg));
        return;
    }
    let use_layers_on = matches!(cfg.use_layers, LayersConfig::On { .. });
    if use_layers_on || cfg.legacy_disable_layers || index == 0 {
        out.push_str(ltr);
    } else {
        push_specificity_level(out, ltr, index);
    }
}

fn compare_rules(
    a: &StylexRule,
    b: &StylexRule,
    cfg: &AssembleConfig,
    cmp: Comparator,
) -> Ordering {
    let diff = a.priority - b.priority;
    if diff != 0.0 {
        return if diff < 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    if cfg.use_legacy_classnames_sort {
        cmp(&a.class_name, &b.class_name)
    } else {
        match cmp(decl_slice(&a.ltr), decl_slice(&b.ltr)) {
            Ordering::Equal => cmp(&a.ltr, &b.ltr),
            other => other,
        }
    }
}

fn assemble_impl(
    rules: &[StylexRule],
    cfg: &AssembleConfig,
    custom_cmp: Option<Comparator>,
) -> Result<String, AssembleError> {
    if rules.is_empty() {
        return Ok(String::new());
    }

    let consts_timer = timings::start(Stage::AssembleConsts);
    let consts =
        collect_resolved_consts(rules.iter().filter(|r| is_const_rule(r)).map(const_decl))?;
    let prepared_consts = prepare_consts(&consts);
    let non_const: Vec<&StylexRule> = rules.iter().filter(|r| !is_const_rule(r)).collect();
    drop(consts_timer);

    let sorted: Vec<&StylexRule> = match custom_cmp {
        Some(cmp) => {
            let _t = timings::start(Stage::AssembleSort);
            let mut refs = non_const;
            refs.sort_by(|a, b| compare_rules(a, b, cfg, cmp));
            refs
        }
        None => keyed_order(non_const, cfg.use_legacy_classnames_sort),
    };

    // Logical-float detection reads the pre-substitution rule text.
    let logical_float_vars = logical_float_block(sorted.iter().any(|r| rule_has_logical_float(r)));

    let subst_timer = timings::start(Stage::AssembleSubst);
    let substituted: Vec<(Option<String>, Option<String>)> = if prepared_consts.is_empty() {
        Vec::new()
    } else {
        sorted
            .iter()
            .map(|r| {
                (
                    substitute_consts(&r.ltr, &prepared_consts),
                    r.rtl
                        .as_deref()
                        .and_then(|t| substitute_consts(t, &prepared_consts)),
                )
            })
            .collect()
    };
    drop(subst_timer);
    let text_of = |i: usize| -> (&str, Option<&str>) {
        let rule = sorted[i];
        let (ltr, rtl) = match substituted.get(i) {
            Some((ltr, rtl)) => (ltr.as_deref(), rtl.as_deref()),
            None => (None, None),
        };
        (ltr.unwrap_or(&rule.ltr), rtl.or(rule.rtl.as_deref()))
    };

    let _render_timer = timings::start(Stage::AssembleRender);
    // Consecutive runs of floor(priority/1000); input is priority-sorted so runs
    // are the priority bands.
    let mut bands: Vec<(usize, usize)> = Vec::new();
    let mut last_level = -1.0f64;
    for (i, rule) in sorted.iter().enumerate() {
        let level = (rule.priority / 1000.0).floor();
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
        .map(|r| r.ltr.len() + r.rtl.as_ref().map_or(0, |t| t.len() + 40) + 1)
        .sum();
    let mut out = String::with_capacity(
        logical_float_vars.len() + header.len() + text_bytes + 32 * bands.len(),
    );
    out.push_str(&logical_float_vars);
    out.push_str(&header);

    let mut slots: Vec<usize> = Vec::new();
    let mut slot_of: FxHashMap<&str, usize> = FxHashMap::default();
    for (index, &(start, end)) in bands.iter().enumerate() {
        // Last-wins dedupe by className, first-occurrence order (JS Map semantics).
        slots.clear();
        slot_of.clear();
        for (offset, rule) in sorted[start..end].iter().enumerate() {
            let i = start + offset;
            match slot_of.entry(&rule.class_name) {
                Entry::Vacant(vacant) => {
                    vacant.insert(slots.len());
                    slots.push(i);
                }
                Entry::Occupied(occupied) => slots[*occupied.get()] = i,
            }
        }

        if index > 0 {
            out.push('\n');
        }
        let wrap = use_layers_on && sorted[start].priority > 0.0;
        if wrap {
            out.push_str("@layer ");
            out.push_str(&layer_name(layer_prefix, index));
            out.push_str("{\n");
        }
        for (j, &i) in slots.iter().enumerate() {
            if j > 0 {
                out.push('\n');
            }
            let (ltr, rtl) = text_of(i);
            // Empty rtl is falsy upstream and behaves exactly like absent.
            push_chunk(&mut out, ltr, rtl.filter(|s| !s.is_empty()), index, cfg);
        }
        if wrap {
            out.push_str("\n}");
        }
    }
    Ok(out)
}

// localeCompare levels of one string laid out [primary n][secondary n][tertiary n];
// the declaration slice is its last `decl` chars, so that key is each level's suffix.
enum RuleKey {
    Verified { n: usize, decl: usize, buf: Vec<u8> },
    DeclOnly { decl: usize, buf: Vec<u8> },
    Fallback,
}

type Segments<'a> = [&'a [u8]; 3];

fn cmp_segments(a: Segments<'_>, b: Segments<'_>) -> Ordering {
    a[0].cmp(b[0])
        .then_with(|| a[1].cmp(b[1]))
        .then_with(|| a[2].cmp(b[2]))
}

impl RuleKey {
    fn for_rule(r: &StylexRule, legacy: bool) -> RuleKey {
        if legacy {
            RuleKey::derive(&r.class_name, 0)
        } else {
            RuleKey::derive(&r.ltr, decl_slice(&r.ltr).len())
        }
    }

    fn derive(s: &str, decl_bytes: usize) -> RuleKey {
        let decl_start = s.len() - decl_bytes;
        if s.is_ascii() {
            let n = s.len();
            let mut buf = vec![0u8; 3 * n];
            for (i, &b) in s.as_bytes().iter().enumerate() {
                match ASCII_LOCALE_KEYS[usize::from(b)] {
                    Some((p, sec, ter)) => {
                        buf[i] = p;
                        buf[n + i] = sec;
                        buf[2 * n + i] = ter;
                    }
                    None => return RuleKey::decl_only(s, decl_start, i),
                }
            }
            return RuleKey::Verified {
                n,
                decl: decl_bytes,
                buf,
            };
        }
        let n = s.chars().count();
        let mut buf = vec![0u8; 3 * n];
        for (i, (at, c)) in s.char_indices().enumerate() {
            match locale_key(c) {
                Ok((p, sec, ter)) => {
                    buf[i] = p;
                    buf[n + i] = sec;
                    buf[2 * n + i] = ter;
                }
                Err(_) => return RuleKey::decl_only(s, decl_start, at),
            }
        }
        RuleKey::Verified {
            n,
            decl: s[decl_start..].chars().count(),
            buf,
        }
    }

    // An unverified char at byte `at`; only a fully verified declaration suffix
    // after it keeps a key.
    fn decl_only(s: &str, decl_start: usize, at: usize) -> RuleKey {
        if at >= decl_start || decl_start == s.len() {
            return RuleKey::Fallback;
        }
        match RuleKey::derive(&s[decl_start..], s.len() - decl_start) {
            RuleKey::Verified { n, buf, .. } => RuleKey::DeclOnly { decl: n, buf },
            _ => RuleKey::Fallback,
        }
    }

    fn verified(&self) -> bool {
        matches!(self, RuleKey::Verified { .. })
    }

    fn decl_segments(&self) -> Option<Segments<'_>> {
        match self {
            RuleKey::Verified { n, decl, buf } => Some([
                &buf[n - decl..*n],
                &buf[2 * n - decl..2 * n],
                &buf[3 * n - decl..],
            ]),
            RuleKey::DeclOnly { decl, buf } => {
                Some([&buf[..*decl], &buf[*decl..2 * decl], &buf[2 * decl..]])
            }
            RuleKey::Fallback => None,
        }
    }

    fn full_segments(&self) -> Option<Segments<'_>> {
        match self {
            RuleKey::Verified { n, buf, .. } => Some([&buf[..*n], &buf[*n..2 * n], &buf[2 * n..]]),
            _ => None,
        }
    }

    // Equal primary sequences imply equal char counts, so the secondary and
    // tertiary segment comparisons are always aligned.
    fn cmp_verified(&self, other: &RuleKey) -> Ordering {
        match (self, other) {
            (
                RuleKey::Verified {
                    n: na,
                    decl: da,
                    buf: ba,
                },
                RuleKey::Verified {
                    n: nb,
                    decl: db,
                    buf: bb,
                },
            ) => ba[na - da..*na]
                .cmp(&bb[nb - db..*nb])
                .then_with(|| ba[2 * na - da..2 * na].cmp(&bb[2 * nb - db..2 * nb]))
                .then_with(|| ba[3 * na - da..].cmp(&bb[3 * nb - db..]))
                .then_with(|| ba[..*na].cmp(&bb[..*nb]))
                .then_with(|| ba[*na..2 * na].cmp(&bb[*nb..2 * nb]))
                .then_with(|| ba[2 * na..].cmp(&bb[2 * nb..])),
            _ => unreachable!("cmp_verified on a fallback collation key"),
        }
    }

    // default_locale_cmp semantics per step: either side unverified → UTF-16 order.
    fn cmp_with_fallback(
        &self,
        other: &RuleKey,
        a: &StylexRule,
        b: &StylexRule,
        legacy: bool,
    ) -> Ordering {
        if legacy {
            return match (self.full_segments(), other.full_segments()) {
                (Some(ka), Some(kb)) => cmp_segments(ka, kb),
                _ => utf16_cmp(&a.class_name, &b.class_name),
            };
        }
        let decl = match (self.decl_segments(), other.decl_segments()) {
            (Some(ka), Some(kb)) => cmp_segments(ka, kb),
            _ => utf16_cmp(decl_slice(&a.ltr), decl_slice(&b.ltr)),
        };
        if decl != Ordering::Equal {
            return decl;
        }
        match (self.full_segments(), other.full_segments()) {
            (Some(ka), Some(kb)) => cmp_segments(ka, kb),
            _ => utf16_cmp(&a.ltr, &b.ltr),
        }
    }
}

// Keys derived once per rule; the stable index sort and the per-pair fallback
// keep comparator semantics identical.
fn keyed_order(non_const: Vec<&StylexRule>, legacy: bool) -> Vec<&StylexRule> {
    let dedupe_timer = timings::start(Stage::AssembleDedupe);
    let mut unique = dedupe_keep_first(&non_const);
    drop(dedupe_timer);
    let keys_timer = timings::start(Stage::AssembleKeys);
    let mut keys: Vec<RuleKey> = unique
        .iter()
        .map(|r| RuleKey::for_rule(r, legacy))
        .collect();
    // Dropping duplicates is order-neutral only under a consistent total order
    // (verified keys, finite priorities); otherwise sort the full input as before.
    if keys
        .iter()
        .zip(&unique)
        .any(|(k, r)| !k.verified() || !r.priority.is_finite())
    {
        unique = non_const;
        keys = unique
            .iter()
            .map(|r| RuleKey::for_rule(r, legacy))
            .collect();
    }
    drop(keys_timer);
    let _sort_timer = timings::start(Stage::AssembleSort);
    let mut order: Vec<u32> = (0..unique.len() as u32).collect();
    order.sort_by(|&ia, &ib| {
        let (a, b) = (unique[ia as usize], unique[ib as usize]);
        let diff = a.priority - b.priority;
        if diff != 0.0 {
            return if diff < 0.0 {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        keys[ia as usize].cmp_with_fallback(&keys[ib as usize], a, b, legacy)
    });
    order.into_iter().map(|i| unique[i as usize]).collect()
}

// Keep-first is output-neutral (copies sort Equal, adjacent); a class with two
// distinct texts keeps every copy, since a copy between them decides last-wins.
fn dedupe_keep_first<'a>(rules: &[&'a StylexRule]) -> Vec<&'a StylexRule> {
    let mut seen: FxHashSet<(&str, &str, Option<&str>, u64)> = FxHashSet::default();
    let first: Vec<bool> = rules
        .iter()
        .map(|r| {
            seen.insert((
                &r.class_name,
                &r.ltr,
                r.rtl.as_deref(),
                r.priority.to_bits(),
            ))
        })
        .collect();
    if seen.len() == rules.len() {
        return rules.to_vec();
    }
    let mut texts: FxHashMap<&str, u32> = FxHashMap::default();
    for (r, _) in rules.iter().zip(&first).filter(|(_, fresh)| **fresh) {
        *texts.entry(&r.class_name).or_default() += 1;
    }
    let single_text_classes = texts.len() == seen.len();
    rules
        .iter()
        .zip(first)
        .filter(|(r, fresh)| *fresh || (!single_text_classes && texts[&*r.class_name] > 1))
        .map(|(r, _)| *r)
        .collect()
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

fn resolve_constant(
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
        let replacement = resolve_constant(&ref_val.clone(), consts, visited)?;
        let needle = result[start..end].to_string();
        result = js_replace_first(&result, &needle, &replacement.to_js_string());
        visited.retain(|v| *v != ref_name);
        scan_from = 0;
    }
    Ok(ConstVal::Str(result))
}

// Scanner for /var\((--[A-Za-z0-9_-]+)\)/ from a byte offset; returns the byte
// span of the whole match.
fn find_var_ref(s: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = s.as_bytes();
    let mut pos = from;
    loop {
        let off = s.get(pos..)?.find("var(--")?;
        let start = pos + off;
        let mut j = start + 6;
        while j < bytes.len() && is_var_ident_byte(bytes[j]) {
            j += 1;
        }
        if j > start + 6 && j < bytes.len() && bytes[j] == b')' {
            return Some((start, j + 1));
        }
        pos = start + 1;
    }
}

fn is_var_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

// The per-const strings substitution needs, derived once per assemble instead
// of once per rule × const.
struct PreparedConst {
    var_ref: String,
    replacement: String,
    key_rewrite: Option<(String, String)>,
}

fn prepare_consts(consts: &[(String, ConstVal)]) -> Vec<PreparedConst> {
    consts
        .iter()
        .map(|(var_ref, const_val)| {
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
                    let const_name = &var_ref[4..var_ref.len() - 1];
                    (format!("{const_name}:"), format!("{target_name}:"))
                });
            PreparedConst {
                var_ref: var_ref.clone(),
                replacement,
                key_rewrite,
            }
        })
        .collect()
}

// The contains gates only skip js_replace_all calls that would be identity
// copies; a hit re-probes for var(-- because replacements can add/remove refs.
/// `None` when no const reference matched: callers keep sharing the rule's
/// Arc'd text instead of materializing an identical String.
fn substitute_consts(text: &str, consts: &[PreparedConst]) -> Option<String> {
    if consts.is_empty() {
        return None;
    }
    let mut out: Option<String> = None;
    let mut has_ref = text.contains("var(--");
    for c in consts {
        let cur = out.as_deref().unwrap_or(text);
        if has_ref && cur.contains(c.var_ref.as_str()) {
            let next = js_replace_all(cur, &c.var_ref, &c.replacement);
            has_ref = next.contains("var(--");
            out = Some(next);
        }
        let cur = out.as_deref().unwrap_or(text);
        if let Some((needle, target)) = &c.key_rewrite
            && cur.contains(needle.as_str())
        {
            let next = js_replace_all(cur, needle, target);
            has_ref = next.contains("var(--");
            out = Some(next);
        }
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

fn js_replace_first(s: &str, needle: &str, replacement: &str) -> String {
    let Some(pos) = s.find(needle) else {
        return s.to_string();
    };
    let mut out = Vec::with_capacity(s.len());
    out.extend_from_slice(&s.as_bytes()[..pos]);
    js_substitution(&mut out, s, pos, needle, replacement);
    out.extend_from_slice(&s.as_bytes()[pos + needle.len()..]);
    String::from_utf8(out).expect("byte-splices preserve UTF-8")
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

// :not(#\#) polyfill; inserted before the first '::' when present, else before
// the last '{'; @keyframes exempt (only @keyframes — @property etc. are not).
fn add_specificity_level(selector: &str, index: usize) -> String {
    let mut out = String::with_capacity(selector.len() + 9 * index);
    push_specificity_level(&mut out, selector, index);
    out
}

fn push_specificity_level(out: &mut String, selector: &str, index: usize) {
    if selector.starts_with("@keyframes") {
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
// not @keyframes); None routes the rule through push_chunk.
fn plain_split(ltr: &str, rtl: Option<&str>) -> Option<usize> {
    if rtl.is_some_and(|t| !t.is_empty()) || ltr.contains(", .") || ltr.starts_with("@keyframes") {
        return None;
    }
    Some(specificity_split(ltr))
}

fn add_ancestor_selector(selector: &str, ancestor_selector: &str) -> String {
    if selector.starts_with("@keyframes") {
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
        // indexOf miss → -1 → slice(0,0) prefix and the whole string as rest.
        None => format!("{ancestor_selector} {selector}"),
    }
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

// The one comparator shared with pseudo sorting (jsrt::default_locale_cmp);
// re-exported so existing callers keep their import path.
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
    priority: f64,
    key: RuleKey,
    // ltr/rtl carry the const substitutions; unchanged text shares the rule's Arc.
    ltr: Arc<str>,
    rtl: Option<Arc<str>>,
    plain_split: Option<usize>,
}

struct FilePrep {
    rules: PreparedRules,
    const_decls: Vec<(String, ConstVal)>,
    // safe = every sort key verified and every priority finite: the exact
    // preconditions under which sorted-by-key order equals the stable sort.
    safe: bool,
    logical_float: bool,
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

type PreparedPool = HashMap<u64, Vec<Weak<SharedPreparedRule>>, BuildHasherDefault<IdentityHasher>>;

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

fn prepare_rule(r: &StylexRule, legacy: bool, consts: &[PreparedConst]) -> PreparedRule {
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
        priority: r.priority,
        key: RuleKey::for_rule(r, legacy),
        plain_split: plain_split(&ltr, rtl.as_deref()),
        ltr,
        rtl,
    }
}

fn prepare_file(
    rules: &[StylexRule],
    legacy: bool,
    consts: &[PreparedConst],
    pool: &mut Option<PreparedPool>,
) -> FilePrep {
    let mut prep = FilePrep {
        rules: if pool.is_some() {
            PreparedRules::Shared(Vec::with_capacity(rules.len()))
        } else {
            PreparedRules::Inline(Vec::with_capacity(rules.len()))
        },
        const_decls: Vec::new(),
        safe: true,
        logical_float: false,
    };
    for r in rules {
        if is_const_rule(r) {
            prep.const_decls.push(const_decl(r));
            continue;
        }
        prep.logical_float |= rule_has_logical_float(r);
        match &mut prep.rules {
            PreparedRules::Inline(rules) => rules.push(prepare_rule(r, legacy, consts)),
            PreparedRules::Shared(rules) => {
                let bucket = pool
                    .as_mut()
                    .expect("shared preparation pool")
                    .entry(class_hash64(&r.class_name))
                    .or_default();
                let shared = bucket
                    .iter()
                    .filter_map(Weak::upgrade)
                    .find(|p| p.source == *r)
                    .unwrap_or_else(|| {
                        let shared = Arc::new(SharedPreparedRule {
                            source: r.clone(),
                            prepared: prepare_rule(r, legacy, consts),
                        });
                        bucket.push(Arc::downgrade(&shared));
                        shared
                    });
                rules.push(shared);
            }
        }
        let prepared = &prep.rules[prep.rules.len() - 1];
        prep.safe &= prepared.priority.is_finite() && prepared.key.verified();
    }
    prep
}

// Strict total order = comparator order + the (file, seq) input position; with
// safe files this reproduces the one-shot stable sort exactly.
fn entry_cmp(files: &[FilePrep], a: (u32, u32), b: (u32, u32)) -> Ordering {
    let pa = &files[a.0 as usize].rules[a.1 as usize];
    let pb = &files[b.0 as usize].rules[b.1 as usize];
    let diff = pa.priority - pb.priority;
    if diff != 0.0 {
        return if diff < 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    pa.key.cmp_verified(&pb.key).then_with(|| a.cmp(&b))
}

type RuleOwners = FxHashMap<Arc<str>, Vec<(u32, u32)>>;

struct IncrState {
    cfg: AssembleConfig,
    dirty: BTreeSet<String>,
    structural: bool,
    consts: Vec<(String, ConstVal)>,
    prepared_consts: Vec<PreparedConst>,
    paths: Vec<String>,
    files: Vec<FilePrep>,
    pool: Option<PreparedPool>,
    sorted: Vec<(u32, u32)>,
    owners: Option<RuleOwners>,
}

const MAX_INCR_STATES: usize = 4;

// Incremental assembly for the HMR path (design-core.md §7): file-keyed rule
// sets, canonical emission order = BTreeMap file order, memoized full assemble.
#[derive(Default)]
pub struct RuleRegistry {
    files: BTreeMap<String, Vec<StylexRule>>,
    generation: u64,
    cache: Option<(u64, AssembleConfig, Result<String, AssembleError>)>,
    incr: Vec<IncrState>,
}

impl RuleRegistry {
    pub fn new() -> Self {
        RuleRegistry::default()
    }

    pub fn set_file_rules(&mut self, file: &str, rules: Vec<StylexRule>) {
        if self.files.get(file).is_some_and(|old| *old == rules) {
            return;
        }
        let existed = self.files.insert(file.to_string(), rules).is_some();
        for state in &mut self.incr {
            if existed {
                state.dirty.insert(file.to_string());
            } else {
                state.structural = true;
            }
        }
        self.generation += 1;
    }

    pub fn remove_file(&mut self, file: &str) -> bool {
        let removed = self.files.remove(file).is_some();
        if removed {
            for state in &mut self.incr {
                state.structural = true;
            }
            self.generation += 1;
        }
        removed
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
        let prepared = self.try_incremental(cfg).or_else(|| {
            self.rebuild_incr_state(cfg);
            self.try_incremental(cfg)
        });
        let result = match prepared {
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
            None => assemble(&self.all_rules(), cfg),
        };
        self.cache = Some((self.generation, cfg.clone(), result.clone()));
        result
    }

    // The fast path: re-prepare only the swapped files and splice them into the
    // cached sort order. None = fall back to the one-shot path.
    fn try_incremental(&mut self, cfg: &AssembleConfig) -> Option<String> {
        let Self { files, incr, .. } = self;
        let state = incr.iter_mut().find(|s| s.cfg == *cfg)?;
        if state.structural {
            return None;
        }

        let mut raw_consts: Vec<(String, ConstVal)> = Vec::new();
        for (rank, path) in state.paths.iter().enumerate() {
            if state.dirty.contains(path) {
                let rules = files.get(path)?;
                raw_consts.extend(rules.iter().filter(|r| is_const_rule(r)).map(const_decl));
            } else {
                raw_consts.extend(state.files[rank].const_decls.iter().cloned());
            }
        }
        let resolved = collect_resolved_consts(raw_consts.into_iter()).ok()?;
        if resolved != state.consts {
            return None;
        }

        let dirty = std::mem::take(&mut state.dirty);
        let mut changed = vec![false; state.files.len()];
        let mut prepared = Vec::with_capacity(dirty.len());
        let mut classes = FxHashSet::default();
        for path in &dirty {
            let rank = state.paths.binary_search(path).ok()? as u32;
            let prep = prepare_file(
                files.get(path)?,
                state.cfg.use_legacy_classnames_sort,
                &state.prepared_consts,
                &mut state.pool,
            );
            if !prep.safe {
                return None;
            }
            changed[rank as usize] = true;
            if state.owners.is_some() {
                for file in [&state.files[rank as usize], &prep] {
                    for seq in 0..file.rules.len() {
                        classes.insert(file.rules[seq].class_name.clone());
                    }
                }
            }
            prepared.push((rank, prep));
        }
        if !prepared.is_empty() {
            state.sorted.retain(|&(rank, seq)| {
                if state.owners.is_some() {
                    !classes.contains(&state.files[rank as usize].rules[seq as usize].class_name)
                } else {
                    !changed[rank as usize]
                }
            });
            if let Some(owners) = &mut state.owners {
                for class in &classes {
                    if let Some(entries) = owners.get_mut(class) {
                        entries.retain(|&(rank, _)| !changed[rank as usize]);
                    }
                }
            }
            for (rank, prep) in prepared {
                state.files[rank as usize] = prep;
            }
            let mut fresh = Vec::new();
            for (rank, &is_changed) in changed.iter().enumerate() {
                if !is_changed {
                    continue;
                }
                for seq in 0..state.files[rank].rules.len() {
                    let entry = (rank as u32, seq as u32);
                    if let Some(owners) = &mut state.owners {
                        insert_owner(owners, &state.files, entry)?;
                    } else {
                        fresh.push(entry);
                    }
                }
            }
            if let Some(owners) = &mut state.owners {
                for class in classes {
                    let entries = owners.get_mut(&class)?;
                    if entries.is_empty() {
                        owners.remove(&class);
                    } else {
                        entries.sort_unstable();
                        fresh.push(entries[0]);
                    }
                }
            }
            fresh.sort_unstable_by(|&a, &b| entry_cmp(&state.files, a, b));
            state.sorted = merge_sorted(&state.files, &state.sorted, fresh);
        }
        if let Some(pool) = &mut state.pool {
            let mut unique = 0;
            pool.retain(|_, bucket| {
                bucket.retain(|rule| rule.strong_count() != 0);
                unique += bucket.len();
                !bucket.is_empty()
            });
            let occurrences: usize = state.files.iter().map(|file| file.rules.len()).sum();
            if unique * 2 > occurrences {
                return None;
            }
        }

        if files.values().all(|rules| rules.is_empty()) {
            return Some(String::new());
        }
        Some(render(state))
    }

    // Full rebuild of the per-config state; drops it instead when the rule set
    // is outside the safe preconditions (or consts fail to resolve).
    fn rebuild_incr_state(&mut self, cfg: &AssembleConfig) {
        self.incr.retain(|s| s.cfg != *cfg);
        let raw = self
            .files
            .values()
            .flat_map(|rules| rules.iter().filter(|r| is_const_rule(r)).map(const_decl));
        let Ok(consts) = collect_resolved_consts(raw) else {
            return;
        };
        let prepared_consts = prepare_consts(&consts);
        let mut paths = Vec::with_capacity(self.files.len());
        let mut fps = Vec::with_capacity(self.files.len());
        let mut pool = prefer_shared_preparation(&self.files).then(PreparedPool::default);
        for (path, rules) in &self.files {
            let prep = prepare_file(
                rules,
                cfg.use_legacy_classnames_sort,
                &prepared_consts,
                &mut pool,
            );
            if !prep.safe {
                return;
            }
            paths.push(path.clone());
            fps.push(prep);
        }
        let mut sorted: Vec<(u32, u32)> = fps
            .iter()
            .enumerate()
            .flat_map(|(rank, f)| (0..f.rules.len()).map(move |seq| (rank as u32, seq as u32)))
            .collect();
        let owners = pool.as_ref().and_then(|_| {
            let mut owners = FxHashMap::default();
            for &entry in &sorted {
                insert_owner(&mut owners, &fps, entry)?;
            }
            Some(owners)
        });
        if let Some(owners) = &owners {
            sorted = owners.values().map(|entries| entries[0]).collect();
        }
        sorted.sort_unstable_by(|&a, &b| entry_cmp(&fps, a, b));
        if self.incr.len() >= MAX_INCR_STATES {
            self.incr.remove(0);
        }
        self.incr.push(IncrState {
            cfg: cfg.clone(),
            dirty: BTreeSet::new(),
            structural: false,
            consts,
            prepared_consts,
            paths,
            files: fps,
            pool,
            sorted,
            owners,
        });
    }
}

fn insert_owner(owners: &mut RuleOwners, files: &[FilePrep], entry: (u32, u32)) -> Option<()> {
    let rule = &files[entry.0 as usize].rules[entry.1 as usize];
    let entries = owners.entry(rule.class_name.clone()).or_default();
    if let Some(&(rank, seq)) = entries.first() {
        let previous = &files[rank as usize].rules[seq as usize];
        // Conflicting class definitions retain upstream's first-slot/last-value semantics.
        if previous.priority != rule.priority
            || previous.key.cmp_verified(&rule.key) != Ordering::Equal
            || previous.ltr != rule.ltr
            || previous.rtl != rule.rtl
        {
            return None;
        }
    }
    entries.push(entry);
    Some(())
}

fn merge_sorted(
    files: &[FilePrep],
    kept: &[(u32, u32)],
    fresh: Vec<(u32, u32)>,
) -> Vec<(u32, u32)> {
    let mut merged = Vec::with_capacity(kept.len() + fresh.len());
    let mut prev = 0usize;
    for &candidate in &fresh {
        let at = prev
            + kept[prev..].partition_point(|&e| entry_cmp(files, e, candidate) == Ordering::Less);
        merged.extend_from_slice(&kept[prev..at]);
        merged.push(candidate);
        prev = at;
    }
    merged.extend_from_slice(&kept[prev..]);
    merged
}

// First slot inline: an empty Vec never allocates, so only a real 64-bit hash
// collision costs a heap allocation.
type SlotMap = HashMap<u64, (u32, Vec<u32>), BuildHasherDefault<IdentityHasher>>;

fn render(state: &IncrState) -> String {
    let IncrState {
        cfg, files, sorted, ..
    } = state;

    struct GroupMeta {
        first_priority: f64,
        emit: Vec<(u32, u32)>,
    }
    let mut groups: Vec<GroupMeta> = Vec::new();
    let mut slots: Vec<(u32, u32)> = Vec::new();
    let mut slot_map: SlotMap = SlotMap::default();
    let mut last_band = 0.0f64;
    let mut first_priority = 0.0f64;
    let mut open = false;
    for &(rank, seq) in sorted.iter() {
        let p = &files[rank as usize].rules[seq as usize];
        let band = (p.priority / 1000.0).floor();
        if !open || band != last_band {
            if open {
                groups.push(GroupMeta {
                    first_priority,
                    emit: std::mem::take(&mut slots),
                });
            }
            slot_map.clear();
            open = true;
            last_band = band;
            first_priority = p.priority;
        }
        // JS Map semantics: a repeated class keeps its first slot, latest value wins.
        match slot_map.entry(p.class_hash) {
            std::collections::hash_map::Entry::Vacant(vacant) => {
                vacant.insert((slots.len() as u32, Vec::new()));
                slots.push((rank, seq));
            }
            std::collections::hash_map::Entry::Occupied(mut occupied) => {
                let same_class = |&si: &u32| {
                    let (er, es) = slots[si as usize];
                    files[er as usize].rules[es as usize].class_name == p.class_name
                };
                let (first, rest) = occupied.get_mut();
                let existing = std::iter::once(*first).find(|si| same_class(si));
                let existing = existing.or_else(|| rest.iter().copied().find(|si| same_class(si)));
                match existing {
                    Some(si) => slots[si as usize] = (rank, seq),
                    None => {
                        rest.push(slots.len() as u32);
                        slots.push((rank, seq));
                    }
                }
            }
        }
    }
    if open {
        groups.push(GroupMeta {
            first_priority,
            emit: slots,
        });
    }

    let (use_layers_on, _, _, layer_prefix) = layer_parts(cfg);
    let header = if use_layers_on {
        layers_header(cfg, groups.len())
    } else {
        String::new()
    };
    let logical = logical_float_block(files.iter().any(|f| f.logical_float));

    let mut out = String::with_capacity(logical.len() + header.len() + 64 * sorted.len().min(8192));
    out.push_str(&logical);
    out.push_str(&header);
    for (index, group) in groups.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let wrap = use_layers_on && group.first_priority > 0.0;
        if wrap {
            out.push_str("@layer ");
            out.push_str(&layer_name(layer_prefix, index));
            out.push_str("{\n");
        }
        for (j, &(rank, seq)) in group.emit.iter().enumerate() {
            if j > 0 {
                out.push('\n');
            }
            let p = &files[rank as usize].rules[seq as usize];
            match p.plain_split {
                Some(_) if use_layers_on || cfg.legacy_disable_layers || index == 0 => {
                    out.push_str(&p.ltr);
                }
                Some(split) => push_split(&mut out, &p.ltr, split, index),
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
}
