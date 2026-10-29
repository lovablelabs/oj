//! tsrs-only: `TSRS_TRACE_UNION_REDUCTION=1` (docs/DEBUGGING.md, notes/open-history-dependence.md section 4). Lines on
//! stderr for the two places that report TS2590, and for the order in which each checker checks its files:
//!
//! - `tsrs union reduction`: every subtype reduction of a union of more than 1,000 types. Only those can reach the
//!   limit in `remove_subtypes` (its estimate is at most `(length - 1) * length`, which exceeds 1,000,000 from 1,001
//!   types on).
//! - `tsrs cross product`: every `check_cross_product_union` whose size is at least 1,000 (TS2590 from 100,000 on):
//!   intersections of unions, spreads and template literals over unions, tuples with a union in a variadic element.
//! - `tsrs check file`: every file a checker begins to check, and every piece of a split declaration file. Neither
//!   TS2590 site caches its failure, but caches above them keep the failed result (the error type for an intersection
//!   split in halves, a relation decided against the error type), so a checker reports such a TS2590 only where it
//!   first builds the type; these lines show what it had checked before.
//!
//! Every line names the checker by its id. The first two say where the error would be reported and what was decided,
//! and fingerprint the inputs, so that two runs that disagree on a TS2590 show which input differed: the constituents
//! (`set`), their order (`order`), or, for a reduction, the relation results (which constituents were removed,
//! `removed`). The fingerprints are built from what a type already holds (flags, symbol names, declaration files by
//! base name and positions, resolved type arguments, literal values), never from type ids, which follow the order in
//! which a checker created its types, and without resolving anything, so that tracing does not change what it traces
//! (it only fills the cached line map of the file it reports a position in). A deferred type reference whose
//! arguments are not resolved yet is described without them, so `set` can differ between two runs whose unions are
//! the same.

use std::fmt::Write;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::OnceLock;

use crate::*;

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("TSRS_TRACE_UNION_REDUCTION").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

/// The 100,000-comparison checkpoint of `remove_subtypes`: sources begun so far and the estimate taken there.
pub struct Checkpoint {
    pub(crate) sources: i64,
    pub(crate) estimate: i64,
}

/// One trace line for the reduction of `input` (in the union's order) that kept `kept`: everything the reduction
/// returned, or what was left when it gave up with TS2590. `comparisons` is `None` for an answer from the cache.
pub fn report(
    c: &Checker,
    input: &[P<Type>],
    kept: &[P<Type>],
    verdict: &str,
    comparisons: Option<i64>,
    checkpoint: Option<Checkpoint>,
) {
    let described: Vec<String> = input.iter().map(|&t| describe(t)).collect();
    let mut sorted = described.clone();
    sorted.sort_unstable();
    let kept: FxHashSet<P<Type>> = kept.iter().copied().collect();
    let mut removed: Vec<&str> = input
        .iter()
        .zip(&described)
        .filter(|(t, _)| !kept.contains(t))
        .map(|(_, d)| d.as_str())
        .collect();
    removed.sort_unstable();
    let mut line = format!(
        "tsrs union reduction: checker {} at {} while checking {} | {} -> {} types, {verdict}",
        c.id,
        location(c.current_node),
        checking_file(c),
        input.len(),
        kept.len()
    );
    match comparisons {
        Some(n) => {
            let _ = write!(line, " | {n} comparisons");
        }
        None => line.push_str(" | from the cache"),
    }
    if let Some(cp) = checkpoint {
        let _ = write!(line, ", at 100000: {} sources begun, estimate {}", cp.sources, cp.estimate);
    }
    let _ = write!(
        line,
        " | set {:016x} order {:016x} removed {:016x}",
        fingerprint(&sorted),
        fingerprint(&described),
        fingerprint(&removed)
    );
    let head: Vec<&str> = described.iter().take(3).map(String::as_str).collect();
    let tail: Vec<&str> = described.iter().rev().take(3).rev().map(String::as_str).collect();
    let _ = write!(line, " | first [{}] last [{}]", head.join("; "), tail.join("; "));
    eprintln!("{line}");
}

/// One trace line for a cross product of `types` that `check_cross_product_union` sized at `size`: each union is
/// described by its length, fingerprints and first two members, any other type by `describe`.
pub fn report_cross_product(c: &Checker, types: &[P<Type>], size: usize) {
    // The limit of `check_cross_product_union`.
    let verdict = if size >= 100_000 { "too complex (TS2590)" } else { "allowed" };
    let mut line = format!(
        "tsrs cross product: checker {} at {} while checking {} | size {size}, {verdict}",
        c.id,
        location(c.current_node),
        checking_file(c)
    );
    for &t in types {
        if t.flags().intersects(TypeFlags::Union) {
            let described: Vec<String> = t.types().iter().map(|&m| describe(m)).collect();
            let mut sorted = described.clone();
            sorted.sort_unstable();
            let head: Vec<&str> = described.iter().take(2).map(String::as_str).collect();
            let _ = write!(
                line,
                " | union {} set {:016x} order {:016x} first [{}]",
                described.len(),
                fingerprint(&sorted),
                fingerprint(&described),
                head.join("; ")
            );
        } else {
            let _ = write!(line, " | {}", describe(t));
        }
    }
    eprintln!("{line}");
}

/// One trace line when checker `c` begins to check `file`, or the range of its statements `piece` when it checks a
/// piece of a split declaration file (`check_source_file_piece`).
pub fn report_check_file(c: &Checker, file: P<SourceFile>, piece: Option<&std::ops::Range<usize>>) {
    match piece {
        Some(statements) => eprintln!(
            "tsrs check file: checker {} begins statements {}..{} of {}",
            c.id,
            statements.start,
            statements.end,
            relative(file.file_name())
        ),
        None => {
            eprintln!("tsrs check file: checker {} begins {}", c.id, relative(file.file_name()));
        }
    }
}

fn checking_file(c: &Checker) -> String {
    c.checking_file.map_or_else(|| "-".to_string(), |f| relative(f.file_name()).to_string())
}

fn fingerprint<S: AsRef<str>>(items: &[S]) -> u64 {
    // DefaultHasher::new() has fixed keys: the same items hash the same in every run.
    let mut h = DefaultHasher::new();
    for item in items {
        item.as_ref().hash(&mut h);
    }
    h.finish()
}

/// `name` relative to the current directory when it is below it, as diagnostics print it.
fn relative(name: &str) -> &str {
    static CWD: OnceLock<String> = OnceLock::new();
    let cwd = CWD.get_or_init(|| {
        std::env::current_dir().map(|d| format!("{}/", d.display())).unwrap_or_default()
    });
    name.strip_prefix(cwd.as_str()).unwrap_or(name)
}

fn location(node: Option<P<Node>>) -> String {
    let Some(node) = node else {
        return "-".to_string();
    };
    let Some(file) = ast::get_source_file_of_node(node) else {
        return "-".to_string();
    };
    let start = tsrs_scanner::get_token_pos_of_node(node, file, false);
    let (line, character) =
        tsrs_scanner::get_ecma_line_and_utf16_character_of_position(&*file, start);
    format!("{}({},{})", relative(file.file_name()), line + 1, character + 1)
}

fn describe(t: P<Type>) -> String {
    let mut out = String::new();
    describe_into(t, 3, &mut out);
    out
}

fn describe_into(t: P<Type>, depth: u32, out: &mut String) {
    let _ = write!(out, "{:x}", t.flags().bits());
    if t.flags().intersects(TypeFlags::Object) {
        let _ = write!(out, "/{:x}", (t.object_flags() & ObjectFlags::ObjectTypeKindMask).bits());
    }
    if let Some(symbol) = t.alias().and_then(|a| a.symbol.get()) {
        out.push_str(" alias ");
        describe_symbol(symbol, out);
    }
    if let Some(symbol) = t.symbol() {
        out.push(' ');
        describe_symbol(symbol, out);
    }
    if t.flags().intersects(TypeFlags::Literal)
        && let Some(value) = t.as_literal_type().value.get()
    {
        let _ = write!(out, " {value:?}");
    }
    if depth == 0 {
        return;
    }
    let nested: Option<&[P<Type>]> = if t.flags().intersects(TypeFlags::Object)
        && t.object_flags().intersects(ObjectFlags::Reference)
    {
        let reference = t.as_type_reference();
        if let Some(symbol) = reference.target.get().and_then(|target| target.symbol()) {
            out.push_str(" of ");
            describe_symbol(symbol, out);
        }
        reference.resolved_type_arguments.get()
    } else if t.flags().intersects(TypeFlags::UnionOrIntersection) {
        Some(t.types())
    } else {
        None
    };
    if let Some(nested) = nested {
        out.push('<');
        for (i, &n) in nested.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            describe_into(n, depth - 1, out);
        }
        out.push('>');
    }
}

// The declaration's file by its base name, so that the fingerprints do not depend on the directory tsrs runs in.
fn describe_symbol(symbol: P<Symbol>, out: &mut String) {
    out.push_str(symbol.name());
    if let Some(&declaration) = symbol.declarations().first()
        && let Some(file) = ast::get_source_file_of_node(declaration)
    {
        let name = file.file_name();
        let _ = write!(out, "@{}:{:?}", name.rsplit('/').next().unwrap_or(name), declaration.pos());
    }
}
