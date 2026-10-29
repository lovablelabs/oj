// tsrs-only (notes/perf-next-heavy-files.md): checking one heavy declaration file on several checkers.
//
// Stealing moves whole files, so with many checkers the type-check pass cannot end before its costliest file does. In
// Next.js's `packages/next` that file is a checked declaration file: `@modelcontextprotocol/sdk`'s `types.d.ts`
// (249k nodes of inferred Zod types), a fifth of a one-checker check, alone on its checker for the whole pass at 32
// checkers. Its cost is TypeScript's: each `z.ZodObject<{ ... }>` reference relates a fresh type literal to the
// `ZodRawShape` constraint, comparing every Zod schema class in it to `ZodTypeAny` member by member (tsgo takes twice
// as long on it, with the same type and instantiation counts).
//
// So the type-check pass with stealing cuts such a file into contiguous statement ranges ("pieces"). The checker that
// runs the file's own queue item (the owner) checks the first piece; each other piece is a queue item of its own, at
// the front of another checker's queue, and that checker runs `Checker::check_source_file_piece` on it: the statements
// of the range and the nodes they deferred, nothing file-level. A piece nobody has started when the owner is done
// with its own is run by the owner. The owner then waits for the pieces in flight (they never wait on anything, so
// this cannot deadlock), adds their diagnostics to its collection (`add_piece_diagnostics`) and runs the normal
// per-file step, whose `check_source_file` does the file-level checks (grammar, deferred nodes, module exports) and
// skips the statements. The file's checker for later passes stays the owner.
//
// Why the output is the same: a file's diagnostics are its checker's collection for the file, deduplicated and sorted
// (`DiagnosticsCollection`). Output already does not depend on which checker checks a file or in what order a checker
// visits files (notes/perf-order-independence.md, the `random:<seed>` check): the diagnostics found for a node are a
// function of the program, not of what the checker resolved before. A piece is the same thing at statement
// granularity, a checker visiting some statements of a file without the others. Only declaration files are split:
// they have no function bodies, so no control flow, no unreachable-code or unused-identifier checks, and nothing
// later passes need from the checker that checked them (they are not emitted, and have no declaration diagnostics).
// Default library files are split only when they weigh most of a share (`LIB_MIN_SHARE_PERCENT`, `plan_splits`): at half a
// share (webpack at 16 checkers) splitting lib.dom.d.ts cost 3-4% of wall time, at a whole share (webpack and
// next-packages-next at 32) not splitting it made it the pass's tail.
//
// `TSRS_SPLIT_FILES` (read once): unset = on in the stealing pass; `0`/`off`; a comma list of `shadow` (the pieces run
// as usual, then the owner also checks the other checkers' pieces itself and reports the file as one checker finds
// it; it panics if the split would have reported something else, see `shadow_compare`) and `force:<k>`
// (split every checked declaration file with at least two statements into up to k pieces, whatever its weight: the
// test mode for the conformance suite and corpora) and `stats` (one line per pass on stderr: files split, pieces, pieces
// another checker ran; `stats:<file>` appends it to the file, for harnesses that keep stderr). `TSRS_SPLIT_MIN_SHARE=<percent>` sets which files
// are split (weight at least that percentage of an average checker's share, default `MIN_SHARE_PERCENT`) and
// `TSRS_SPLIT_PIECE_DIVISOR=<n>` the piece size (1/n of a share, default `PIECE_DIVISOR`), for experiments.

use std::ops::Range;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use tsrs_ast::{Diagnostic, Kind, SourceFile, compare_diagnostics, equal_diagnostics};
use tsrs_core::P;

use crate::checkerpool::{Checker, Context};

// A declaration file is split when its static weight (node count, text, imports) is at least 40% of an average
// checker's share of the pass, into pieces of about 1/PIECE_DIVISOR of a share. The weight underestimates declaration
// files like the MCP one about fourfold (5.4% of next's weight, 23% of its one-checker check CPU), so a file that
// weighs under half a share can still be the pass's tail: at 8 checkers the MCP file is 43% of a share and takes the
// whole check phase. Lighter declaration files cost about their weight (lib.dom.d.ts: 2.7% of the weight, 3% of the
// CPU), and splitting them only adds work: each piece costs the checker that runs it the library types it touches. On
// the 64-vCPU runner, 40% against 75%: next 0.294 against 0.398 s of check time at 8 checkers, 0.173 against 0.187 at
// 16, the same at 32 and 64; webpack (which splits lib.dom.d.ts) the same check time with 1-4% more instructions.
// Pieces of 1/8 of a share bring the MCP file's largest piece from 0.17 to 0.085 s at 16 checkers on a Mac (1/16 does
// not go lower). Measured in the note.
const MIN_SHARE_PERCENT: u64 = 40;
/// The share a default library file must weigh to be split (`plan_splits`): its weight is a fair estimate of its cost,
/// so splitting pays only when it would be most of a checker's work (webpack and next-packages-next at 32 checkers),
/// not at half a share (webpack at 16: +3-4% wall when it was split).
pub const LIB_MIN_SHARE_PERCENT: u64 = 80;
const PIECE_DIVISOR: u64 = 8;

pub struct SplitConfig {
    pub(crate) enabled: bool,
    pub(crate) shadow: bool,
    pub(crate) force: Option<usize>,
    pub(crate) divisor: u64,
    pub(crate) min_share_percent: u64,
    // `stats`: Some(None) prints to stderr, Some(Some(path)) appends to the file.
    #[expect(
        clippy::option_option,
        reason = "Distinguishes disabled statistics, stderr output, and output to a file."
    )]
    stats: Option<Option<String>>,
}

pub fn split_config() -> &'static SplitConfig {
    static CONFIG: OnceLock<SplitConfig> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let mut config = SplitConfig { enabled: true, shadow: false, force: None, divisor: PIECE_DIVISOR, min_share_percent: MIN_SHARE_PERCENT, stats: None };
        if let Ok(value) = std::env::var("TSRS_SPLIT_FILES") {
            for token in value.split(',').map(str::trim).filter(|t| !t.is_empty()) {
                match token {
                    "0" | "off" => config.enabled = false,
                    "1" | "on" => {}
                    "shadow" => config.shadow = true,
                    "stats" => config.stats = Some(None),
                    _ if token.starts_with("stats:") => config.stats = Some(Some(token["stats:".len()..].to_string())),
                    _ => match token.strip_prefix("force:").and_then(|k| k.parse::<usize>().ok()) {
                        Some(k) if k >= 2 => config.force = Some(k),
                        _ => panic!("TSRS_SPLIT_FILES: unknown value {token:?} (expected 0, off, shadow, stats, stats:<file>, force:<k> with k >= 2)"),
                    },
                }
            }
        }
        if let Some(divisor) = std::env::var("TSRS_SPLIT_PIECE_DIVISOR").ok().and_then(|v| v.parse::<u64>().ok()).filter(|&d| d > 0) {
            config.divisor = divisor;
        }
        if let Some(percent) = std::env::var("TSRS_SPLIT_MIN_SHARE").ok().and_then(|v| v.parse::<u64>().ok()) {
            config.min_share_percent = percent;
        }
        config
    })
}

// Whether every top-level statement of `file` is a declaration. Checking a statement may leave state on the nodes it
// shares with the file's other statements, and for top-level statements that is only the source file node. One such
// state decides a diagnostic by visiting order: `checkGrammarStatementInAmbientContext` reports "Statements are not
// allowed in ambient contexts" on the first executable statement of a block only (`hasReportedStatementInAmbientContext`
// on the block), so two pieces would each report their first one. Only executable statements (blocks, expression
// statements, `if`, loops, `return`, `throw`, `try`, ...) run that check, so a file whose top-level statements are all
// declarations is split, and any other is not. (The other first-visitor guards a declaration's check runs are per
// symbol and either pick the first declaration in source order, such as overload and merged-export checks, or report
// the same diagnostics whichever checker runs them, such as identical type parameters of merged interfaces; the
// collection's deduplication absorbs the latter.)
fn all_declarations(file: P<SourceFile>) -> bool {
    file.statements.nodes().iter().all(|s| {
        matches!(
            s.kind(),
            Kind::InterfaceDeclaration
                | Kind::TypeAliasDeclaration
                | Kind::ClassDeclaration
                | Kind::FunctionDeclaration
                | Kind::VariableStatement
                | Kind::EnumDeclaration
                | Kind::ModuleDeclaration
                | Kind::ImportDeclaration
                | Kind::ImportEqualsDeclaration
                | Kind::ExportDeclaration
                | Kind::ExportAssignment
                | Kind::NamespaceExportDeclaration
        )
    })
}

// Contiguous statement ranges of about equal text length, at most `count` of them; none (no split) when there would be
// fewer than two or a top-level statement is not a declaration (`all_declarations`).
pub fn statement_pieces(file: P<SourceFile>, count: usize) -> Vec<Range<usize>> {
    let statements = file.statements.nodes();
    let count = count.min(statements.len());
    if count < 2 || !all_declarations(file) {
        return Vec::new();
    }
    let lengths: Vec<u64> =
        statements.iter().map(|s| u64::from(s.loc().len().unwrap_or(0).max(1))).collect();
    let total: u64 = lengths.iter().sum();
    let mut pieces = Vec::with_capacity(count);
    let (mut start, mut sum) = (0, 0u64);
    for (i, &len) in lengths.iter().enumerate() {
        sum += len;
        // Close piece k once the running length reaches k/count of the total, leaving a statement for each piece left.
        let k = pieces.len() as u64 + 1;
        if sum * count as u64 >= total * k
            && statements.len() - (i + 1) >= count - pieces.len() - 1
            && pieces.len() + 1 < count
        {
            pieces.push(start..i + 1);
            start = i + 1;
        }
    }
    pieces.push(start..statements.len());
    if pieces.len() < 2 {
        return Vec::new();
    }
    pieces
}

const NOT_STARTED: u8 = 0;
const RUNNING: u8 = 1;
const DONE: u8 = 2;

// One file checked in pieces during one pass.
pub struct SplitFile {
    pub(crate) file: P<SourceFile>,
    pieces: Vec<Range<usize>>,
    // Per piece: NOT_STARTED, RUNNING, DONE. Piece 0 is the owner's and never queued.
    states: Vec<AtomicU8>,
    // The diagnostics and suggestions of each finished piece, with the piece and whether another checker ran it.
    results: Mutex<Vec<PieceResult>>,
    finished: Condvar,
    shadow: bool,
    // Shadow mode: the diagnostics the owner found in the pieces other checkers ran, when it checked them itself.
    owner_found: Mutex<Vec<P<Diagnostic>>>,
}

struct PieceResult {
    piece: usize,
    by_other: bool,
    diagnostics: Vec<P<Diagnostic>>,
    suggestions: Vec<P<Diagnostic>>,
}

impl SplitFile {
    pub(crate) fn new(file: P<SourceFile>, pieces: Vec<Range<usize>>, shadow: bool) -> SplitFile {
        let states = pieces.iter().map(|_| AtomicU8::new(NOT_STARTED)).collect();
        SplitFile {
            file,
            pieces,
            states,
            results: Mutex::new(Vec::new()),
            finished: Condvar::new(),
            shadow,
            owner_found: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn is_shadow(&self) -> bool {
        self.shadow
    }

    // The pieces that are queue items of their own: all but the owner's first one.
    pub(crate) fn queued_pieces(&self) -> Range<usize> {
        1..self.pieces.len()
    }

    fn claim(&self, k: usize) -> bool {
        // The exchange only decides who runs the piece; its results are published under `results`' mutex.
        self.states[k]
            .compare_exchange(NOT_STARTED, RUNNING, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    fn finish(&self, result: PieceResult) {
        let k = result.piece;
        let mut results = self.results.lock().unwrap();
        results.push(result);
        // Read under the same mutex by `wait_for_pieces`, which is what orders it after `results`.
        self.states[k].store(DONE, Ordering::Relaxed);
        self.finished.notify_all();
    }

    // A piece's queue item: runs the piece unless the owner already took it.
    pub(crate) fn run_queued_piece(&self, checker: &mut Checker, ctx: &Context, k: usize) {
        if self.claim(k) {
            let (diagnostics, suggestions) =
                checker.check_source_file_piece(ctx, self.file, self.pieces[k].clone());
            self.finish(PieceResult { piece: k, by_other: true, diagnostics, suggestions });
        }
    }

    // The owner's step before the normal per-file callback: its own piece, the pieces nobody started, then (after the
    // pieces in flight finished) the other checkers' diagnostics. In shadow mode the owner instead checks the other
    // checkers' pieces itself too, so the callback reports the file as one checker finds it.
    pub(crate) fn run_owner(&self, checker: &mut Checker, ctx: &Context) {
        checker.check_source_file_piece(ctx, self.file, self.pieces[0].clone());
        for k in self.queued_pieces() {
            if self.claim(k) {
                // On the owner's own checker: its diagnostics are in the collection already.
                checker.check_source_file_piece(ctx, self.file, self.pieces[k].clone());
                self.finish(PieceResult {
                    piece: k,
                    by_other: false,
                    diagnostics: Vec::new(),
                    suggestions: Vec::new(),
                });
            }
        }
        let results = self.wait_for_pieces();
        if self.shadow {
            let mut by_other: Vec<usize> =
                results.iter().filter(|r| r.by_other).map(|r| r.piece).collect();
            drop(results);
            by_other.sort_unstable();
            let mut found = Vec::new();
            found.extend(by_other.into_iter().flat_map(|k| {
                let (added, _) =
                    checker.check_source_file_piece(ctx, self.file, self.pieces[k].clone());
                added
            }));
            *self.owner_found.lock().unwrap() = found;
            checker.add_piece_diagnostics(self.file, &[], &[]);
            return;
        }
        let diagnostics: Vec<P<Diagnostic>> =
            results.iter().flat_map(|r| r.diagnostics.iter().copied()).collect();
        let suggestions: Vec<P<Diagnostic>> =
            results.iter().flat_map(|r| r.suggestions.iter().copied()).collect();
        drop(results);
        checker.add_piece_diagnostics(self.file, &diagnostics, &suggestions);
    }

    // Shadow mode, after the callback: `whole` is the owner's collection for the file, every statement checked by the
    // owner. The split check reports the owner's collection without what it found in the other checkers' pieces, plus
    // those checkers' diagnostics. That equals `whole` when (a) every diagnostic another checker found is in `whole` and
    // (b) every diagnostic the owner found in another checker's piece was found by some other checker too.
    pub(crate) fn shadow_compare(&self, whole: &[P<Diagnostic>]) {
        let results = self.wait_for_pieces();
        let others: Vec<P<Diagnostic>> = results
            .iter()
            .filter(|r| r.by_other)
            .flat_map(|r| r.diagnostics.iter().copied())
            .collect();
        drop(results);
        let contains = |list: &[P<Diagnostic>], d: P<Diagnostic>| {
            list.iter().any(|&e| {
                compare_diagnostics(e, d) == std::cmp::Ordering::Equal && equal_diagnostics(e, d)
            })
        };
        let describe = |d: P<Diagnostic>| {
            format!(
                "TS{} at {:?}: {:?}",
                d.code(),
                d.pos(),
                d.message().map(tsrs_diagnostics::Message::text)
            )
        };
        if let Some(&d) = others.iter().find(|&&d| !contains(whole, d)) {
            panic!(
                "TSRS_SPLIT_FILES=shadow: {} in {} pieces: another checker reports {}, which the unsplit check does not",
                self.file.file_name(),
                self.pieces.len(),
                describe(d)
            );
        }
        if let Some(&d) = self.owner_found.lock().unwrap().iter().find(|&&d| !contains(&others, d))
        {
            panic!(
                "TSRS_SPLIT_FILES=shadow: {} in {} pieces: the unsplit check reports {}, which the checker of its piece does not",
                self.file.file_name(),
                self.pieces.len(),
                describe(d)
            );
        }
    }

    pub(crate) fn report_stats(files: &[SplitFile]) {
        let Some(target) = &split_config().stats else {
            return;
        };
        if files.is_empty() {
            return;
        }
        let (pieces, by_other) =
            files.iter().map(SplitFile::piece_counts).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        let line = format!(
            "split check: {} files, {pieces} pieces, {by_other} run by another checker\n",
            files.len()
        );
        match target {
            None => eprint!("{line}"),
            Some(path) => {
                use std::io::Write;
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .expect("TSRS_SPLIT_FILES=stats:<file>");
                file.write_all(line.as_bytes()).expect("TSRS_SPLIT_FILES=stats:<file>");
            }
        }
    }

    // (pieces, pieces another checker ran), after the pass.
    fn piece_counts(&self) -> (usize, usize) {
        (self.pieces.len(), self.results.lock().unwrap().iter().filter(|r| r.by_other).count())
    }

    fn wait_for_pieces(&self) -> std::sync::MutexGuard<'_, Vec<PieceResult>> {
        let mut results = self.results.lock().unwrap();
        // Written under this mutex by `finish`.
        while self.queued_pieces().any(|k| self.states[k].load(Ordering::Relaxed) != DONE) {
            results = self.finished.wait(results).unwrap();
        }
        results
    }
}
