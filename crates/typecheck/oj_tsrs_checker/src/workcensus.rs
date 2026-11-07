//! Attributes checker work to source-level patterns.
//!
//! Work census (`TSRS_WORK_CENSUS=<output file>`, docs/DEBUGGING.md): attributes checker work to source-level identities (the conditional type's root alias, the callee's declaration, the relation target's symbol, the flow container) instead of to functions.
//!
//! Off unless the variable is set; every hook first tests `Checker::census`.
//!
//! Timing: each hooked call is a span on a stack. A span's inclusive time counts toward its category only when no
//! other span of that category encloses it, and toward its key only when no span with the same key encloses it, so
//! totals do not double count recursion; self time excludes every nested span. Spans read `Instant` (~12 ns on Apple
//! Silicon; thread CPU time costs ~105 ns a read, too much per span, and with one checker the two agree). Each span's
//! bookkeeping after it ends is measured and charged to the enclosing span; the remaining per-span cost is calibrated
//! at startup and subtracted.

use crate::checker::Checker;
use crate::types::{ConditionalRoot, Type};
use rustc_hash::{FxHashMap, FxHashSet};
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use tsrs_ast as ast;
use tsrs_ast::{Node, Symbol};
use tsrs_core::P;

/// `TSRS_WORK_CENSUS_SLOW`=<ms>: print spans of a few categories that take longer (diagnosis only).
pub(crate) fn slow_threshold_ns() -> u64 {
    static NS: OnceLock<u64> = OnceLock::new();
    *NS.get_or_init(|| {
        std::env::var("TSRS_WORK_CENSUS_SLOW")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map_or(u64::MAX, |ms| (ms * 1e6) as u64)
    })
}

/// The output file; None unless the binary was built with `-p tsrs_checker --features tsrs_checker/work-census` and `TSRS_WORK_CENSUS` is set.
pub(crate) fn census_path() -> Option<&'static str> {
    static PATH: OnceLock<Option<String>> = OnceLock::new();

    if !cfg!(feature = "work-census") {
        return None;
    }
    PATH.get_or_init(|| {
        std::env::var("TSRS_WORK_CENSUS").ok().filter(|v| !v.is_empty() && v != "0")
    })
    .as_deref()
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
#[repr(u8)]
pub enum Cat {
    File,
    CondInst,
    Cond,
    MappedResolve,
    MappedProp,
    Call,
    Infer,
    CtxCheck,
    Rel,
    Flow,
    Union,
    RemoveSubtypes,
    IndexedAccess,
    KeyOf,
    ExportAssign,
    VarInit,
    RelUnion,
}

const CAT_COUNT: usize = 17;

impl Cat {
    fn name(self) -> &'static str {
        match self {
            Cat::File => "checkSourceFile",
            Cat::CondInst => "conditional instantiation (miss path, incl. distribution)",
            Cat::Cond => "getConditionalType",
            Cat::MappedResolve => "resolveMappedTypeMembers",
            Cat::MappedProp => "getTypeOfMappedSymbol",
            Cat::Call => "resolveCall",
            Cat::Infer => "inferTypeArguments",
            Cat::CtxCheck => "checkExpressionWithContextualType",
            Cat::Rel => "structuredTypeRelatedTo",
            Cat::Flow => "getFlowTypeOfReference",
            Cat::Union => "getUnionType (worker)",
            Cat::RemoveSubtypes => "removeSubtypes",
            Cat::IndexedAccess => "getIndexedAccessType",
            Cat::KeyOf => "getIndexType (keyof)",
            Cat::ExportAssign => "export assignment type (checkExpressionCached)",
            Cat::VarInit => "variable initializer type (checkDeclarationInitializer)",
            Cat::RelUnion => "typeRelatedToSomeType (relation to a union target)",
        }
    }
}

/// What a stat row is keyed by. Names are resolved when the census is collected.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum CKey {
    None,
    Root(P<ConditionalRoot>),
    Node(P<Node>),
    OptNode(Option<P<Node>>),
    /// Relation kind (0 assignable, 1 comparable, 2 subtype, 3 strict subtype, 4 identity, 5 other), the target's
    /// alias or type symbol, and a type-kind label index when there is no symbol.
    Rel(u8, Option<P<Symbol>>, u8),
    /// Size bucket (log2 of the size, rounded up).
    Bucket(u32),
    /// A mapped type (its declaration node) with the bucket of its key count.
    Mapped(Option<P<Node>>, u32),
}

#[derive(Default, Clone, Copy)]
pub struct Stat {
    pub(crate) count: u64,
    outer: u64,
    pub(crate) incl_ns: u64,
    /// Inclusive time of the spans not enclosed by a span with the same (category, active key).
    pub(crate) key_incl_ns: u64,
    pub(crate) self_ns: u64,
    pub(crate) a: u64,
    pub(crate) b: u64,
    c: u64,
}

#[derive(Clone, Copy)]
pub struct Span {
    cat: Cat,
    key: CKey,
}

#[derive(Clone, Copy)]
pub struct Timing {
    pub(crate) incl_ns: u64,
    pub(crate) self_ns: u64,
    pub(crate) outer: bool,
    pub(crate) key_outer: bool,
}

struct Frame {
    start: u64,
    child_ns: u64,
    child_frames: u64,
    /// Time the nested spans spent in census bookkeeping after their end (their `record`), measured.
    overhead_ns: u64,
}

pub struct Census {
    epoch: Instant,
    pair_ns: f64,
    pending_end: u64,
    stack: Vec<Frame>,
    depth: [u32; CAT_COUNT],
    active: FxHashMap<(Cat, CKey), u32>,
    frames: u64,
    pub(crate) stats: FxHashMap<(Cat, CKey), Stat>,
    /// Distribution fan-out per conditional root and size bucket: count, constituents, never results, time.
    pub(crate) fanout: FxHashMap<(P<ConditionalRoot>, u32), Stat>,
    /// structuredTypeRelatedTo pairs (source id, target id) -> relation kinds seen.
    pub(crate) rel_pairs: FxHashMap<(u32, u32), u8>,
    /// recursiveTypeRelatedTo cache per relation kind: hit, miss, maybe-stack hit.
    pub(crate) rel_cache: [[u64; 3]; 6],
    /// inferTypeArguments keyed by (signature, argument types, check mode, contextual type): count, time, first
    /// call node, repeats at another call node.
    pub(crate) infer_keys: FxHashMap<u128, (u32, u64, usize, u32, Option<P<Node>>)>,
    pub(crate) infer_args: Vec<u32>,
    /// Flow: steps walked in the current invocations (stack), per-invocation histogram, sampled re-walks.
    pub(crate) flow_steps: u64,
    pub(crate) flow_stack: Vec<(u64, u64, Option<u64>)>,
    pub(crate) flow_hist: [Stat; 24],
    pub(crate) flow_seen: FxHashSet<(u64, usize)>,
    pub(crate) flow_sampled_steps: u64,
    pub(crate) flow_sampled_repeats: u64,
    pub(crate) flow_sampled_invocations: u64,
    /// typeRelatedToSomeType outcomes: (source kind, key property state, exit) -> count, constituents tried, time.
    pub(crate) rel_union: FxHashMap<(u8, u8, u8), Stat>,
    /// typeRelatedToSomeType loops: (union size bucket, 0 late success / 1 late success an index would find / 2 no
    /// success) -> calls, failed checks, of which cache hits, time in failed checks.
    pub(crate) rel_union_late: FxHashMap<(u8, u8), Stat>,
    /// The callee of the innermost resolveCall (for checkExpressionWithContextualType).
    pub(crate) call_stack: Vec<Option<P<Node>>>,
    pub(crate) file_wall_ns: u64,
}

pub(crate) fn bucket(n: usize) -> u32 {
    if n <= 1 { 0 } else { (n - 1).bit_width() }
}

fn bucket_label(b: u32) -> String {
    if b == 0 { "<=1".to_string() } else { format!("{}..{}", (1usize << (b - 1)) + 1, 1usize << b) }
}

impl Census {
    pub(crate) fn new() -> Box<Census> {
        let mut c = Box::new(Census {
            epoch: Instant::now(),
            pair_ns: 0.0,
            pending_end: 0,
            stack: Vec::with_capacity(256),
            depth: [0; CAT_COUNT],
            active: FxHashMap::default(),
            frames: 0,
            stats: FxHashMap::default(),
            fanout: FxHashMap::default(),
            rel_pairs: FxHashMap::default(),
            rel_cache: [[0; 3]; 6],
            infer_keys: FxHashMap::default(),
            infer_args: Vec::new(),
            flow_steps: 0,
            flow_stack: Vec::new(),
            flow_hist: [Stat::default(); 24],
            flow_seen: FxHashSet::default(),
            flow_sampled_steps: 0,
            flow_sampled_repeats: 0,
            flow_sampled_invocations: 0,
            call_stack: Vec::new(),
            rel_union: FxHashMap::default(),
            rel_union_late: FxHashMap::default(),
            file_wall_ns: 0,
        });
        // Calibrate what a nested span costs its parent beyond the measured bookkeeping (the clock reads at its
        // edges): run empty spans with a record each inside an outer span.
        let outer = c.begin(Cat::File, CKey::None);
        let n = 200_000u64;
        for i in 0..n {
            let s = c.begin(Cat::Union, CKey::Bucket((i % 4) as u32));
            let t = c.end(s);
            c.record(Cat::Union, CKey::Bucket((i % 4) as u32), t, 0, 0, 0);
        }
        let t = c.end(outer);
        c.pair_ns = t.incl_ns as f64 / n as f64;
        c.frames = 0;
        c.stats.clear();
        c
    }

    #[inline]
    fn now(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    #[inline]
    fn begin(&mut self, cat: Cat, key: CKey) -> Span {
        self.depth[cat as usize] += 1;
        *self.active.entry((cat, key)).or_insert(0) += 1;
        let start = self.now();
        self.stack.push(Frame { start, child_ns: 0, child_frames: 0, overhead_ns: 0 });
        Span { cat, key }
    }

    #[inline]
    fn end(&mut self, span: Span) -> Timing {
        let now = self.now();
        self.pending_end = now;
        let f = self.stack.pop().unwrap();
        let raw = now.saturating_sub(f.start);
        let overhead = (f.child_frames as f64 * self.pair_ns) as u64 + f.overhead_ns;
        let incl = raw.saturating_sub(overhead);
        let self_ns = incl.saturating_sub(f.child_ns);
        if let Some(parent) = self.stack.last_mut() {
            parent.child_ns += incl;
            parent.child_frames += 1 + f.child_frames;
        }
        self.frames += 1;
        let d = &mut self.depth[span.cat as usize];
        *d -= 1;
        let outer = *d == 0;
        let a = self.active.get_mut(&(span.cat, span.key)).unwrap();
        *a -= 1;
        let key_outer = *a == 0;
        if key_outer {
            self.active.remove(&(span.cat, span.key));
        }
        Timing { incl_ns: incl, self_ns, outer, key_outer }
    }

    pub(crate) fn record(&mut self, cat: Cat, key: CKey, t: Timing, a: u64, b: u64, c: u64) {
        let s = self.stats.entry((cat, key)).or_default();
        s.count += 1;
        if t.outer {
            s.outer += 1;
            s.incl_ns += t.incl_ns;
        }
        if t.key_outer {
            s.key_incl_ns += t.incl_ns;
        }
        s.self_ns += t.self_ns;
        s.a += a;
        s.b += b;
        s.c += c;
        self.charge_bookkeeping();
    }

    /// Charges the time since the last span's end (its record) to the enclosing span as overhead.
    #[inline]
    pub(crate) fn charge_bookkeeping(&mut self) {
        let now = self.now();
        let spent = now.saturating_sub(self.pending_end);
        self.pending_end = now;
        if let Some(parent) = self.stack.last_mut() {
            parent.overhead_ns += spent;
        }
    }

    #[inline]
    pub(crate) fn now_ns(&self) -> u64 {
        self.now()
    }

    /// Charges the time since `t0` to the innermost open span as overhead.
    #[inline]
    pub(crate) fn charge_since(&mut self, t0: u64) {
        let now = self.now();
        if let Some(parent) = self.stack.last_mut() {
            parent.overhead_ns += now.saturating_sub(t0);
        }
    }
}

pub(crate) fn rel_kind(c: &Checker, rel: P<crate::relater_types::Relation>) -> u8 {
    if rel == c.assignable_relation {
        0
    } else if rel == c.comparable_relation {
        1
    } else if rel == c.subtype_relation {
        2
    } else if rel == c.strict_subtype_relation {
        3
    } else if rel == c.identity_relation {
        4
    } else {
        5
    }
}

const REL_NAMES: [&str; 6] =
    ["assignable", "comparable", "subtype", "strictSubtype", "identity", "other"];

const KIND_LABELS: [&str; 12] = [
    "object",
    "union",
    "intersection",
    "literal",
    "type-parameter",
    "indexed-access",
    "conditional",
    "substitution",
    "index",
    "template-literal",
    "primitive",
    "other",
];

pub(crate) fn type_identity(t: P<Type>) -> (Option<P<Symbol>>, u8) {
    use crate::types::TypeFlags;

    if let Some(alias) = t.alias()
        && let Some(s) = alias.symbol()
    {
        return (Some(s), 0);
    }
    if let Some(s) = t.symbol() {
        return (Some(s), 0);
    }
    let f = t.flags();
    let k = if f.intersects(TypeFlags::Object) {
        0
    } else if f.intersects(TypeFlags::Union) {
        1
    } else if f.intersects(TypeFlags::Intersection) {
        2
    } else if f.intersects(TypeFlags::Literal) {
        3
    } else if f.intersects(TypeFlags::TypeParameter) {
        4
    } else if f.intersects(TypeFlags::IndexedAccess) {
        5
    } else if f.intersects(TypeFlags::Conditional) {
        6
    } else if f.intersects(TypeFlags::Substitution) {
        7
    } else if f.intersects(TypeFlags::Index) {
        8
    } else if f.intersects(TypeFlags::TemplateLiteral) {
        9
    } else if f.intersects(TypeFlags::Primitive) {
        10
    } else {
        11
    };
    (None, k)
}

fn file_and_line(node: P<Node>) -> String {
    let Some(sf) = ast::get_source_file_of_node(node) else {
        return "?".to_string();
    };
    let pos = if node.pos().is_synthetic() {
        0
    } else {
        tsrs_scanner::get_token_pos_of_node(node, sf, false)
    };
    let line = tsrs_scanner::get_ecma_line_of_position(&*sf, pos);
    let name = sf.file_name();
    let short: String = if let Some(i) = name.find("/node_modules/") {
        name[i + 1..].to_string()
    } else {
        // The last three path components.
        let start = name.rmatch_indices('/').nth(2).map_or(0, |(i, _)| i + 1);
        name[start..].to_string()
    };
    format!("{short}:{}", line + 1)
}

fn symbol_label(s: P<Symbol>) -> String {
    let name = s.name();
    match s.declarations().first() {
        Some(&d) => format!("{name} ({})", file_and_line(d)),
        None => name.to_string(),
    }
}

fn enclosing_declaration_name(node: P<Node>) -> String {
    let n = Some(node);
    if let Some(result) = std::iter::successors(n, |cur| cur.parent()).find_map(|cur| {
        if matches!(
            cur.kind(),
            ast::Kind::TypeAliasDeclaration
                | ast::Kind::InterfaceDeclaration
                | ast::Kind::FunctionDeclaration
                | ast::Kind::MethodDeclaration
                | ast::Kind::MethodSignature
                | ast::Kind::ClassDeclaration
                | ast::Kind::VariableDeclaration
                | ast::Kind::PropertySignature
                | ast::Kind::PropertyDeclaration
        ) && let Some(name) = ast::get_name_of_declaration(cur)
        {
            return Some(tsrs_scanner::get_text_of_node(name));
        }

        None
    }) {
        return result;
    }
    "?".to_string()
}

fn node_label(node: P<Node>) -> String {
    format!("{} @ {}", enclosing_declaration_name(node), file_and_line(node))
}

impl CKey {
    fn label(self) -> String {
        match self {
            CKey::None => "-".to_string(),
            CKey::Root(root) => {
                let node_part = root.node.get().map(node_label).unwrap_or_default();
                match root.alias.get().and_then(|a| a.symbol()) {
                    Some(s) => format!("alias {} | {node_part}", s.name()),
                    None => format!("(no alias) {node_part}"),
                }
            }
            CKey::Node(n) => node_label(n),
            CKey::OptNode(n) => n.map_or_else(|| "(no declaration)".to_string(), node_label),
            CKey::Rel(r, s, k) => match s {
                Some(s) => format!("{} -> {}", REL_NAMES[r as usize], symbol_label(s)),
                None => format!("{} -> <{}>", REL_NAMES[r as usize], KIND_LABELS[k as usize]),
            },
            CKey::Bucket(b) => format!("size {}", bucket_label(b)),
            CKey::Mapped(n, b) => format!(
                "{} | keys {}",
                n.map_or_else(|| "?".to_string(), node_label),
                bucket_label(b)
            ),
        }
    }
}

/// Aggregated rows from every checker, keyed by label.
#[derive(Default)]
struct Global {
    total_wall_ns: u64,
    frames: u64,
    pair_ns: f64,
    stats: FxHashMap<(Cat, String), Stat>,
    fanout: FxHashMap<(String, u32), Stat>,
    rel_pairs_total: u64,
    rel_pairs_multi: u64,
    rel_pairs_masks: FxHashMap<u8, u64>,
    rel_cache: [[u64; 3]; 6],
    infer_distinct: u64,
    infer_total: u64,
    infer_repeat_calls: u64,
    infer_other_site_calls: u64,
    infer_other_site_ns: u64,
    infer_repeat_by_callee: FxHashMap<String, Stat>,
    infer_repeat_ns: u64,
    infer_total_ns: u64,
    flow_hist: [Stat; 24],
    flow_sampled_steps: u64,
    flow_sampled_repeats: u64,
    flow_sampled_invocations: u64,
    rel_union: FxHashMap<(u8, u8, u8), Stat>,
    rel_union_late: FxHashMap<(u8, u8), Stat>,
}

static GLOBAL: Mutex<Option<Global>> = Mutex::new(None);

impl Checker {
    pub fn census_enabled() -> bool {
        census_path().is_some()
    }

    pub fn census_write_report() {
        census_report();
    }

    /// Whether the census is recording. A constant false unless built with `-p tsrs_checker --features tsrs_checker/work-census` (the runtime
    /// test alone cost 0.3-0.5% instructions: notes/perf-checker-algorithms.md), so the hooks compile away.
    #[inline]
    pub(crate) fn census_on(&self) -> bool {
        cfg!(feature = "work-census") && self.census.is_some()
    }

    #[inline]
    pub(crate) fn census_mut(&mut self) -> Option<&mut Census> {
        if cfg!(feature = "work-census") { self.census.as_deref_mut() } else { None }
    }

    #[inline]
    pub(crate) fn census_begin(&mut self, cat: Cat, key: impl FnOnce() -> CKey) -> Option<Span> {
        self.census_mut().map(|c| c.begin(cat, key()))
    }

    #[inline]
    pub(crate) fn census_end(&mut self, span: Option<Span>) -> Option<Timing> {
        match (span, self.census_mut()) {
            (Some(span), Some(c)) => Some(c.end(span)),
            _ => None,
        }
    }

    /// Moves this checker's census into the process-wide report (called once per checker at the end).
    pub fn census_collect(&mut self) {
        let Some(c) = self.census.take() else { return };
        let mut guard = GLOBAL.lock().unwrap();
        let g = guard.get_or_insert_with(Global::default);
        g.total_wall_ns += c.file_wall_ns;
        g.frames += c.frames;
        g.pair_ns = c.pair_ns;
        let mut rows: Vec<((Cat, CKey), Stat)> = c.stats.iter().map(|(k, v)| (*k, *v)).collect();
        rows.sort_by_key(|((cat, _), s)| {
            (*cat, std::cmp::Reverse(s.incl_ns), std::cmp::Reverse(s.count))
        });
        for ((cat, key), s) in rows {
            let e = g.stats.entry((cat, key.label())).or_default();
            add_stat(e, &s);
        }
        let mut fan: Vec<((P<ConditionalRoot>, u32), Stat)> =
            c.fanout.iter().map(|(k, v)| (*k, *v)).collect();
        fan.sort_by_key(|(_, s)| std::cmp::Reverse(s.incl_ns));
        for ((root, b), s) in fan {
            let e = g.fanout.entry((CKey::Root(root).label(), b)).or_default();
            add_stat(e, &s);
        }
        g.rel_pairs_total += c.rel_pairs.len() as u64;
        let masks: Vec<u8> = c.rel_pairs.values().copied().collect();
        for m in masks {
            if m.count_ones() > 1 {
                g.rel_pairs_multi += 1;
            }
            *g.rel_pairs_masks.entry(m).or_default() += 1;
        }
        for (i, row) in c.rel_cache.iter().enumerate() {
            for (j, v) in row.iter().enumerate() {
                g.rel_cache[i][j] += v;
            }
        }
        let infer: Vec<(u32, u64, usize, u32, Option<P<Node>>)> =
            c.infer_keys.values().copied().collect();
        let mut by_callee: FxHashMap<Option<P<Node>>, Stat> = FxHashMap::default();
        for (n, ns, _, other, decl) in infer {
            g.infer_distinct += 1;
            g.infer_total += n as u64;
            g.infer_total_ns += ns;
            if n > 1 {
                g.infer_repeat_calls += n as u64 - 1;
                g.infer_repeat_ns += ns / n as u64 * (n as u64 - 1);
                g.infer_other_site_calls += other as u64;
                g.infer_other_site_ns += ns / n as u64 * other as u64;
                let s = by_callee.entry(decl).or_default();
                s.count += n as u64 - 1;
                s.incl_ns += ns / n as u64 * (n as u64 - 1);
            }
        }
        let mut rows: Vec<(Option<P<Node>>, Stat)> = by_callee.into_iter().collect();
        rows.sort_by_key(|(_, s)| std::cmp::Reverse(s.incl_ns));
        for (decl, s) in rows.into_iter().take(200) {
            add_stat(g.infer_repeat_by_callee.entry(CKey::OptNode(decl).label()).or_default(), &s);
        }
        for (i, s) in c.flow_hist.iter().enumerate() {
            add_stat(&mut g.flow_hist[i], s);
        }
        let rl: Vec<((u8, u8), Stat)> = c.rel_union_late.iter().map(|(k, v)| (*k, *v)).collect();
        for (k, s) in rl {
            add_stat(g.rel_union_late.entry(k).or_default(), &s);
        }
        let ru: Vec<((u8, u8, u8), Stat)> = c.rel_union.iter().map(|(k, v)| (*k, *v)).collect();
        for (k, s) in ru {
            add_stat(g.rel_union.entry(k).or_default(), &s);
        }
        g.flow_sampled_steps += c.flow_sampled_steps;
        g.flow_sampled_repeats += c.flow_sampled_repeats;
        g.flow_sampled_invocations += c.flow_sampled_invocations;
    }
}

fn add_stat(e: &mut Stat, s: &Stat) {
    e.count += s.count;
    e.outer += s.outer;
    e.incl_ns += s.incl_ns;
    e.key_incl_ns += s.key_incl_ns;
    e.self_ns += s.self_ns;
    e.a += s.a;
    e.b += s.b;
    e.c += s.c;
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

/// Writes the collected census to the file named by `TSRS_WORK_CENSUS`.
fn census_report() {
    let Some(path) = census_path() else { return };
    let guard = GLOBAL.lock().unwrap();
    let Some(g) = guard.as_ref() else { return };
    let top: usize =
        std::env::var("TSRS_WORK_CENSUS_TOP").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    let total = g.total_wall_ns.max(1);
    let pct = |ns: u64| 100.0 * ns as f64 / total as f64;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# work census\ncheck wall (sum of checkSourceFile) {:.1} ms, spans {}, span cost {:.1} ns (subtracted from parents; total ~{:.0} ms)",
        ms(g.total_wall_ns),
        g.frames,
        g.pair_ns,
        g.frames as f64 * g.pair_ns / 1e6
    );
    // Category totals.
    let mut cats: FxHashMap<Cat, Stat> = FxHashMap::default();
    let all: Vec<(&(Cat, String), &Stat)> = g.stats.iter().collect();
    for ((cat, _), s) in &all {
        add_stat(cats.entry(*cat).or_default(), s);
    }
    let mut cat_rows: Vec<(Cat, Stat)> = cats.into_iter().collect();
    cat_rows.sort_by_key(|(c, _)| *c);
    let _ = writeln!(
        out,
        "\n## categories (incl = outermost span of the category; self = minus every nested span)\n"
    );
    let _ = writeln!(
        out,
        "| category | calls | outer calls | incl ms | incl % | self ms | self % | a | b | c |"
    );
    let _ = writeln!(out, "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    for (cat, s) in &cat_rows {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {:.1} | {:.2} | {:.1} | {:.2} | {} | {} | {} |",
            cat.name(),
            s.count,
            s.outer,
            ms(s.incl_ns),
            pct(s.incl_ns),
            ms(s.self_ns),
            pct(s.self_ns),
            s.a,
            s.b,
            s.c
        );
    }
    let extras: [(Cat, &str, &str, &str); 17] = [
        (Cat::RelUnion, "constituents tried", "matched by key map", "related"),
        (Cat::File, "-", "-", "-"),
        (Cat::CondInst, "distributed constituents", "never results", "distributions"),
        (Cat::Cond, "-", "-", "-"),
        (Cat::MappedResolve, "keys", "-", "-"),
        (Cat::MappedProp, "-", "-", "-"),
        (Cat::Call, "candidates", "generic candidates", "args"),
        (Cat::Infer, "args", "-", "-"),
        (Cat::CtxCheck, "-", "-", "-"),
        (Cat::Rel, "-", "-", "-"),
        (Cat::Flow, "steps", "max steps", "-"),
        (Cat::Union, "input types", "-", "-"),
        (Cat::RemoveSubtypes, "input types", "removed", "-"),
        (Cat::IndexedAccess, "-", "-", "-"),
        (Cat::KeyOf, "-", "-", "-"),
        (Cat::ExportAssign, "-", "-", "-"),
        (Cat::VarInit, "quick type used", "-", "-"),
    ];
    for (cat, a, b, cc) in extras {
        let mut rows: Vec<(&String, &Stat)> =
            all.iter().filter(|((c, _), _)| *c == cat).map(|((_, k), s)| (k, *s)).collect();
        if rows.is_empty() {
            continue;
        }
        rows.sort_by(|x, y| {
            y.1.key_incl_ns
                .cmp(&x.1.key_incl_ns)
                .then(y.1.incl_ns.cmp(&x.1.incl_ns))
                .then(y.1.count.cmp(&x.1.count))
                .then(x.0.cmp(y.0))
        });
        let _ = writeln!(
            out,
            "\n## {} by key (top {top} of {}, by key incl = outermost span of this key)\n",
            cat.name(),
            rows.len()
        );
        let _ = writeln!(
            out,
            "| key | calls | key incl ms | key incl % | cat-outer incl ms | self ms | {a} | {b} | {cc} |"
        );
        let _ = writeln!(out, "| --- | --- | --- | --- | --- | --- | --- | --- | --- |");
        for (k, s) in rows.iter().take(top) {
            let _ = writeln!(
                out,
                "| {} | {} | {:.1} | {:.2} | {:.1} | {:.1} | {} | {} | {} |",
                k.replace('|', "/"),
                s.count,
                ms(s.key_incl_ns),
                pct(s.key_incl_ns),
                ms(s.incl_ns),
                ms(s.self_ns),
                s.a,
                s.b,
                s.c
            );
        }
    }
    // Distribution fan-out.
    let mut fan: Vec<(&(String, u32), &Stat)> = g.fanout.iter().collect();
    fan.sort_by(|x, y| y.1.key_incl_ns.cmp(&x.1.key_incl_ns).then(x.0.cmp(y.0)));
    let _ = writeln!(
        out,
        "\n## conditional distribution by root and fan-out bucket (top {top} of {}, by key incl)\n",
        fan.len()
    );
    let _ = writeln!(
        out,
        "| root | fan-out | distributions | constituents | never | key incl ms | key incl % | self ms |"
    );
    let _ = writeln!(out, "| --- | --- | --- | --- | --- | --- | --- | --- |");
    for ((k, b), s) in fan.iter().take(top) {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {:.1} | {:.2} | {:.1} |",
            k.replace('|', "/"),
            bucket_label(*b),
            s.count,
            s.a,
            s.b,
            ms(s.key_incl_ns),
            pct(s.key_incl_ns),
            ms(s.self_ns)
        );
    }
    let mut by_bucket: FxHashMap<u32, Stat> = FxHashMap::default();
    let fan_all: Vec<(&(String, u32), &Stat)> = g.fanout.iter().collect();
    for ((_, b), s) in fan_all {
        add_stat(by_bucket.entry(*b).or_default(), s);
    }
    let mut bb: Vec<(u32, Stat)> = by_bucket.into_iter().collect();
    bb.sort_by_key(|(b, _)| *b);
    let _ = writeln!(
        out,
        "\n| fan-out | distributions | constituents | never | key incl ms (may nest across roots) |\n| --- | --- | --- | --- | --- |"
    );
    for (b, s) in bb {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {:.1} |",
            bucket_label(b),
            s.count,
            s.a,
            s.b,
            ms(s.key_incl_ns)
        );
    }
    // Relations.
    let _ = writeln!(
        out,
        "\n## relation caches (recursiveTypeRelatedTo)\n\n| relation | hits | misses | maybe-stack hits |\n| --- | --- | --- | --- |"
    );
    for (i, row) in g.rel_cache.iter().enumerate() {
        let _ = writeln!(out, "| {} | {} | {} | {} |", REL_NAMES[i], row[0], row[1], row[2]);
    }
    let _ = writeln!(
        out,
        "\nstructuredTypeRelatedTo distinct (source, target) pairs (1/8 sample) {}, under more than one relation {}",
        g.rel_pairs_total, g.rel_pairs_multi
    );
    let mut masks: Vec<(u8, u64)> = g.rel_pairs_masks.iter().map(|(k, v)| (*k, *v)).collect();
    masks.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (m, n) in masks.iter().take(12) {
        let names: Vec<&str> = (0..6).filter(|i| m & (1 << i) != 0).map(|i| REL_NAMES[i]).collect();
        let _ = writeln!(out, "- {}: {n}", names.join("+"));
    }
    let _ = writeln!(
        out,
        "\n## generic call repeats\n\ninferTypeArguments calls {} ({:.1} ms incl), distinct (signature, argument types, mode, contextual type) keys {}, repeats {} (~{:.1} ms if repeats cost the key's mean), of which at another call node {} (~{:.1} ms)",
        g.infer_total,
        ms(g.infer_total_ns),
        g.infer_distinct,
        g.infer_repeat_calls,
        ms(g.infer_repeat_ns),
        g.infer_other_site_calls,
        ms(g.infer_other_site_ns)
    );
    let mut rows: Vec<(&String, &Stat)> = g.infer_repeat_by_callee.iter().collect();
    rows.sort_by(|x, y| y.1.incl_ns.cmp(&x.1.incl_ns).then(x.0.cmp(y.0)));
    let _ = writeln!(out, "\n| callee | repeated calls | est. ms |\n| --- | --- | --- |");
    for (k, s) in rows.iter().take(25) {
        let _ = writeln!(out, "| {} | {} | {:.1} |", k.replace('|', "/"), s.count, ms(s.incl_ns));
    }
    let _ = writeln!(
        out,
        "\n## flow walks: steps per getFlowTypeOfReference invocation (exclusive of nested invocations)\n\n| steps | invocations | steps total | incl ms |\n| --- | --- | --- | --- |"
    );
    for (i, s) in g.flow_hist.iter().enumerate() {
        if s.count > 0 {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {:.1} |",
                bucket_label(i as u32),
                s.count,
                s.a,
                ms(s.incl_ns)
            );
        }
    }
    let _ = writeln!(
        out,
        "\nsampled (1/16 of containing functions): invocations {}, steps {}, steps on a (key, flow node) seen before {} ({:.1}%)",
        g.flow_sampled_invocations,
        g.flow_sampled_steps,
        g.flow_sampled_repeats,
        100.0 * g.flow_sampled_repeats as f64 / g.flow_sampled_steps.max(1) as f64
    );
    let mut ru: Vec<(&(u8, u8, u8), &Stat)> = g.rel_union.iter().collect();
    ru.sort_by(|x, y| y.1.key_incl_ns.cmp(&x.1.key_incl_ns).then(x.0.cmp(y.0)));
    let _ = writeln!(out, "\n## typeRelatedToSomeType outcomes (source kind / key map / exit)\n");
    let _ = writeln!(
        out,
        "source kinds: 0 fresh object literal, 1 object literal, 2 other object, 3 intersection, 4 primitive/literal, 5 other"
    );
    let _ = writeln!(
        out,
        "key map: 0 union < 10 or not computed, 1 no key property, 2 key property + match related, 3 key property + match failed, 4 key property + no match"
    );
    let _ = writeln!(
        out,
        "exit: 0 contains, 1 primitive-union fast path, 2 by key match, 3 first constituent, 4 within first 10%, 5 later, 6 none (false)\n"
    );
    let _ = writeln!(
        out,
        "| source | key map | exit | calls | constituents tried | incl ms | incl % |\n| --- | --- | --- | --- | --- | --- | --- |"
    );
    for ((sk, km, ex), s) in ru.iter().take(60) {
        let _ = writeln!(
            out,
            "| {sk} | {km} | {ex} | {} | {} | {:.1} | {:.2} |",
            s.count,
            s.a,
            ms(s.key_incl_ns),
            pct(s.key_incl_ns)
        );
    }
    let mut rl: Vec<(&(u8, u8), &Stat)> = g.rel_union_late.iter().collect();
    rl.sort_by_key(|(k, _)| **k);
    let _ = writeln!(
        out,
        "\n## typeRelatedToSomeType constituent loop: failed checks before the related one\n"
    );
    let _ = writeln!(
        out,
        "| union size | outcome | calls | failed checks | of which cache hits | ms in failed checks | % |\n| --- | --- | --- | --- | --- | --- | --- |"
    );
    for ((b, kind), s) in rl {
        let what = match kind {
            0 => "related later, no single-property index",
            1 => "related later, a property index excludes all earlier",
            _ => "none related",
        };
        let _ = writeln!(
            out,
            "| {} | {what} | {} | {} | {} | {:.1} | {:.2} |",
            bucket_label(*b as u32),
            s.count,
            s.a,
            s.b,
            ms(s.incl_ns),
            pct(s.incl_ns)
        );
    }
    std::fs::write(path, out).expect("TSRS_WORK_CENSUS");
}
