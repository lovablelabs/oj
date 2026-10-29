//! tsrs-only: per-file regions in the CLI's `--noEmit` check, and freeing the tree and binder output of a checked
//! "leaf" file as soon as its checker is done with it (notes/mem-free-leaf-files.md, after Bun's `free_tree`).
//!
//! When the CLI turns file regions on (`enable`, before it creates the program), the host parses each TypeScript
//! source file (`.ts`, `.tsx`, `.mts`, `.cts`; not declaration files) that is loaded as a root file of the program and
//! whose path predicts a leaf (`PREDICTED_LEAF_PATTERNS`: tests, specs, stories, mocks) into a scratch region of its
//! own, and the file is bound in that region too (`bind`). Every other file is parsed into the thread arena as before,
//! where the large chunks are huge pages on Linux. The text, the `SourceFile` itself and the diagnostics stay outside
//! the region (the parser and the diagnostics escape the scratch region), so a freed file still has what the report
//! reads: name, text, line map, counters, and its diagnostics, collected before the free.
//!
//! A program created with `ProgramOptions::leaf_files` set marks its leaves right before its type-check pass
//! (`classify`, `SourceFile::is_check_leaf`): type-checked TypeScript modules that no other file refers to and that
//! declare nothing another file can reach. Only a file with a region can be one: a predicted file that is not a leaf
//! keeps its region, a leaf that was not predicted is not freed. After a leaf's semantic diagnostics are collected (the
//! pass's callback), `free` drops its region with `Region::retire_on_free`: its pages go back to the system and its
//! address range is never reused. Every other region lives for the rest of the process, as the thread arenas did.
#![expect(
    clippy::too_long_first_doc_paragraph,
    reason = "Public for the integration tests; the comment keeps its upstream wording."
)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{self as ast, SourceFile, SourceFileParseOptions, SymbolFlags};
use tsrs_core::arena::Region;
use tsrs_core::{P, ScriptKind};

use crate::program::Program;

/// What a program does with its file regions (`ProgramOptions::leaf_files`; `TSRS_FREE_LEAVES`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LeafMode {
    /// Nothing (the default; `TSRS_FREE_LEAVES=0`, and everywhere but the CLI's `--noEmit` check).
    #[default]
    Off,
    /// Leaves are freed once checked (the CLI's default where allowed).
    Free,
    /// Leaves are counted, nothing is freed (`TSRS_FREE_LEAVES=keep`: measures what the regions cost).
    Keep,
}

static REGIONS_ON: AtomicBool = AtomicBool::new(false);
static STATS: AtomicBool = AtomicBool::new(false);
/// `bind_in_file_regions`.
static OWN_FILE_REGIONS: AtomicBool = AtomicBool::new(false);
/// `LeafSettings::every_file`.
static EVERY_FILE: AtomicBool = AtomicBool::new(false);
/// The directory predicted paths are matched relative to (`enable`).
static CURRENT_DIRECTORY: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Path fragments of the files predicted to be leaves (`predicted_leaf`), matched in the path relative to the current
/// directory (so that a checkout under a `test` directory does not match every file). Tests, stories and mocks: on the
/// bench projects nothing imports them and they are nearly all of the leaves' bytes (vscode 99%, t3code-server 97%,
/// formbricks-web and supabase-studio 95%; notes/mem-free-leaf-files.md). Only a predicted file gets a region; every
/// other file is parsed into the thread arena as before this change, where large chunks are huge pages on Linux.
/// Whether a file is a leaf is still decided exactly (`classify`): a predicted file that is not one keeps its region,
/// a leaf that was not predicted is not freed. A root file that nothing else imports cannot be told at parse time:
/// the files that import it may not have been parsed yet.
const PREDICTED_LEAF_PATTERNS: &[&str] =
    &[".test.", ".spec.", "/test/", "/tests/", "/__tests__/", ".stories.", "/__mocks__/"];

thread_local! {
    /// The host is parsing a root file of the program (`parse_task`).
    static ROOT_PARSE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// File regions by file. Filled by the parse workers, read by the binding ones, emptied of leaves by the checkers.
static REGIONS: LazyLock<Mutex<FxHashMap<P<SourceFile>, Region>>> = LazyLock::new(Default::default);

// `TSRS_FREE_LEAVES=stats` / `keep` counters: written before and during the pass, read after its threads joined.
static LEAVES: AtomicUsize = AtomicUsize::new(0);
static LEAF_BYTES: AtomicUsize = AtomicUsize::new(0);
static FREED: AtomicUsize = AtomicUsize::new(0);
static FREED_BYTES: AtomicUsize = AtomicUsize::new(0);
static REGION_FILES: AtomicUsize = AtomicUsize::new(0);
static REGION_BYTES: AtomicUsize = AtomicUsize::new(0);
static REGION_USED: AtomicUsize = AtomicUsize::new(0);
static CHECKED_FILES: AtomicUsize = AtomicUsize::new(0);
static MISSED: AtomicUsize = AtomicUsize::new(0);
static MISSED_NODES: AtomicUsize = AtomicUsize::new(0);
static LEAF_NODES: AtomicUsize = AtomicUsize::new(0);

/// What `TSRS_FREE_LEAVES` asks for where freeing is allowed (`leaf_settings_from_env`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LeafSettings {
    pub mode: LeafMode,
    /// `stats_report` reports.
    pub stats: bool,
    /// Every TypeScript root file gets a region, not only the predicted ones (`PREDICTED_LEAF_PATTERNS`): for
    /// measurement; it frees the leaves the prediction misses, but moves every tree out of the thread arenas.
    pub every_file: bool,
}

/// The most checkers for which leaf freeing is on by default (`leaf_settings_from_env`). Measured with one binary,
/// freeing on against off, 20 interleaved runs (notes/mem-leaf-regions-cost.md): on the 8-vCPU runner within 2% wall
/// time at 1, 4 and 8 checkers for 13-16% less peak memory on vscode; on the 64-vCPU runner within 1% at 16 checkers
/// for 11% less, and +2.4-3.9% at 32 for 10% less (each give-back flushes the TLB of every core running a checker,
/// and more checkers free leaves in a more scattered order). The README bench's +21% on the 8-vCPU runner was a
/// slower runner and a PGO build without its training profiles (#153), not freeing. `TSRS_FREE_LEAVES=1` turns it on
/// at any count.
const MAX_DEFAULT_CHECKERS: usize = 16;

/// Keeps lazy declaration parsing within its supported CLI modes.
///
/// Whether the CLI may parse declaration-file member lists lazily (notes/mem-lazy-dts-members.md):
/// Enabled by `--fast` or `TSRS_LAZY_DTS=1`; the shared-object check freezes the whole program and disables it.
pub fn lazy_dts_allowed() -> bool {
    let enabled = std::env::var_os("TSRS_LAZY_DTS")
        .map_or_else(tsrs_core::fast::enabled, |v| v != "0" && v != "off");
    enabled && !tsrs_core::ptr::shared_check::enabled()
}

/// Enables lazy declaration parsing for subsequent CLI programs.
///
/// Parses and binds declaration-file member lists lazily in every program created from here on, process-wide
/// (the CLI before `new_program` when no declaration file is type-checked; `TSRS_LAZY_DTS=force` in the test
/// runner, to check that forcing every list gives the same baselines). Never in the language server or the
/// API.
pub fn enable_lazy_dts() {
    tsrs_parser::enable_lazy_dts();
    tsrs_binder::enable_lazy_dts();
    // Relaxed: see `LAZY_DTS`.
    LAZY_DTS.store(true, std::sync::atomic::Ordering::Relaxed);
}

// Relaxed (stores and loads): set by the CLI before the program is created.
static LAZY_DTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Before the checkers of a multi-checker pass are created: parses and binds, in parallel on the worker pool,
/// the lazy member lists that every checker asks for at the start, so that the checkers do not wait for one
/// another on them (notes/mem-lazy-dts-members.md, "Shared lists"). Two sets, each a property of the program,
/// not of a run:
///
/// - the global libraries: every declaration file that enters the program through a default lib, a `lib`
///   option or `/// <reference lib>`, an automatic type directive, the `types` option or a
///   `/// <reference types>` directive, and the files those pull in with `/// <reference path>`
///   (`@types/node`'s `index.d.ts` references the rest of the package). Their globals and ambient modules
///   are what every checker resolves first; module-scoped packages that enter only through imports stay lazy;
/// - the interfaces and classes that merge into the global scope more than once (declared in several script
///   files or `declare global` blocks): `initialize_checker` clones the first symbol's tables and merges
///   the others into them.
///
/// Forcing a list early changes nothing observable (`lazylist`). `TSRS_LAZY_DTS_SHARED=0` forces only the
/// merges.
pub(crate) fn force_shared_lists(program: &crate::program::Program) {
    use rayon::prelude::*;

    // Relaxed: see `LAZY_DTS`.
    if !LAZY_DTS.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let mut lists: Vec<P<tsrs_ast::lazylist::LazyNodeList>> = Vec::new();
    if std::env::var_os("TSRS_LAZY_DTS_SHARED").is_none_or(|v| v != "0") {
        lists.extend(global_library_files(program).into_iter().flat_map(|file| {
            file.lazy_lists
                .get()
                .iter()
                .copied()
                .filter(|l| l.state() == tsrs_ast::lazylist::DEFERRED)
        }));
    }
    let mut by_name: FxHashMap<&'static str, Vec<P<tsrs_ast::Symbol>>> = FxHashMap::default();
    let mut add = |table: Option<P<tsrs_ast::SymbolTable>>| {
        if let Some(table) = table {
            table.for_each(|name, symbol| by_name.entry(name).or_default().push(symbol));
        }
    };
    for &file in program.files {
        if !ast::is_external_or_common_js_module(file) {
            add(file.locals());
        }
        for &augmentation in file.module_augmentations() {
            let declaration = augmentation.parent().unwrap();
            if ast::is_global_scope_augmentation(declaration) {
                add(declaration.symbol().and_then(|s| s.exports()));
            }
        }
    }
    // collects lists to force; forcing order cannot be seen
    lists.extend(
        by_name
            .values()
            .filter_map(|symbols| {
                if symbols.len() > 1 {
                    Some(
                        symbols
                            .iter()
                            .filter_map(|s| s.lazy_list())
                            .filter(|l| l.state() == tsrs_ast::lazylist::DEFERRED),
                    )
                } else {
                    None
                }
            })
            .flatten(),
    );
    tsrs_core::phases::time(program.options().diagnostics_enabled(), "Lazy lists: shared", || {
        crate::program::worker_pool().install(|| lists.par_iter().for_each(|l| l.ensure()));
    });
}

/// The declaration files of the global libraries (`force_shared_lists`), in program order.
fn global_library_files(program: &crate::program::Program) -> Vec<P<SourceFile>> {
    use crate::file_include::FileIncludeKind as K;
    let reasons = &program.file_include_data.file_include_reasons;
    let mut global: FxHashSet<&str> = FxHashSet::default();
    let mut by_reference: Vec<(&str, &str)> = Vec::new(); // (referencing file's path, referenced file's path)
    #[expect(
        clippy::iter_over_hash_type,
        reason = "builds a set and an edge list that is closed over below"
    )]
    for (path, rs) in reasons {
        for r in rs {
            match r.kind {
                K::LibFile
                | K::LibReferenceDirective
                | K::AutomaticTypeDirectiveFile
                | K::TypeReferenceDirective => {
                    global.insert(path);
                }
                K::ReferenceFile => by_reference.push((r.as_referenced_file_data().file, path)),
                _ => {}
            }
        }
    }
    loop {
        let before = global.len();
        for &(from, to) in &by_reference {
            if global.contains(from) {
                global.insert(to);
            }
        }
        if global.len() == before {
            break;
        }
    }
    program
        .files
        .iter()
        .copied()
        .filter(|f| f.is_declaration_file() && global.contains(&*f.path().0))
        .collect()
}

/// Reads the leaf-reclamation policy selected for CLI programs.
///
/// `TSRS_FREE_LEAVES`, a comma-separated list: unset frees the predicted leaves when the program gets at most
/// `MAX_DEFAULT_CHECKERS` checkers (`checkers`: the most it can get, `checker_count_upper_bound`); `1` frees
/// them at any count; `0` turns file regions off; `keep` makes the regions (at any count) but frees nothing;
/// `stats` reports (`stats_report`); `all` gives every TypeScript root file a region
/// (`LeafSettings::every_file`), at any count. Off under the debug modes that walk files or checker data
/// after the pass (`TSRS_FILE_TIMES` walks every tree; the work census walks checker tables). Off
/// without compressed pointers on unix (`Region::retire_on_free`), whatever the value.
pub fn leaf_settings_from_env(checkers: usize) -> LeafSettings {
    if crate::Checker::census_enabled() || crate::checkerpool::file_times_path().is_some() {
        return LeafSettings::default();
    }
    // Only where a freed region's pages go back to the system and its range is never reused (`Region::retire_on_free`
    // gives back pages only with compressed pointers on unix): elsewhere a freed region's memory stays mapped with its
    // old contents while the heap buffers its values owned are freed and reused, so a stale read would see a live
    // object, and nothing would be saved.
    if !(tsrs_core::COMPRESSED_PTRS && cfg!(unix)) {
        return LeafSettings::default();
    }
    let mut settings = LeafSettings { mode: LeafMode::Free, stats: false, every_file: false };
    let mut forced = false;
    for word in std::env::var("TSRS_FREE_LEAVES").unwrap_or_default().split(',') {
        match word.trim() {
            "0" | "off" => return LeafSettings::default(),
            "1" | "on" => forced = true,
            "keep" => {
                settings.mode = LeafMode::Keep;
                forced = true;
            }
            "stats" => settings.stats = true,
            "all" => {
                settings.every_file = true;
                forced = true;
            }
            _ => {}
        }
    }
    if !forced && checkers > MAX_DEFAULT_CHECKERS {
        return LeafSettings::default();
    }
    settings
}

/// Gives subsequent CLI programs per-file arena ownership.
///
/// Turns file regions on for the programs created from here on, process-wide (the CLI, before `new_program`;
/// never the language server, the API or the test harnesses). Predicted paths are matched relative to
/// `current_directory`. From now on, `bind` binds a file that was parsed in a registered region of its own in
/// that region, so its binder data is freed with it (a watch session's file regions, `tsrs_execute`
/// watchmem.rs), or outlives the program that bound it (the declaration files a `tsc -b` shares between projects).
/// Opt-in: in a process with other registered regions (API builds), a region containing a file may be held by the
/// building thread.
pub fn bind_in_file_regions() {
    // Relaxed: set before the session parses anything; the binding threads get their work through locks taken after.
    OWN_FILE_REGIONS.store(true, Ordering::Relaxed);
    // The same sessions free their programs' regions, so lazily filled data of the objects outside every region (the
    // command lines) must not go to one of those, which it would until the first file region is created.
    tsrs_core::arena::route_by_owner();
}

pub fn enable(settings: LeafSettings, current_directory: &str) {
    let _ = CURRENT_DIRECTORY.set(current_directory.trim_end_matches('/').to_string());
    // Relaxed (all three): set on the main thread before the program, and any worker that reads them, exists.
    STATS.store(settings.stats, Ordering::Relaxed);
    EVERY_FILE.store(settings.every_file, Ordering::Relaxed);
    REGIONS_ON.store(settings.mode != LeafMode::Off, Ordering::Relaxed);
}

/// Whether `file_name` matches `PREDICTED_LEAF_PATTERNS` (relative to the current directory).
fn predicted_leaf(file_name: &str) -> bool {
    let relative = match CURRENT_DIRECTORY.get() {
        Some(dir) if !dir.is_empty() && file_name.starts_with(dir.as_str()) => {
            &file_name[dir.len()..]
        }
        _ => file_name,
    };
    PREDICTED_LEAF_PATTERNS.iter().any(|pattern| relative.contains(pattern))
}

/// Whether a file of this name and kind gets a region when it is parsed for a root file: a TypeScript file, not a
/// declaration file, predicted to be a leaf (every such file with `every_file`).
fn region_candidate(file_name: &str, script_kind: ScriptKind) -> bool {
    // Relaxed: see `enable`.
    let every_file = EVERY_FILE.load(Ordering::Relaxed);
    // Relaxed: see `enable`.
    REGIONS_ON.load(Ordering::Relaxed)
        && matches!(script_kind, ScriptKind::TS | ScriptKind::TSX)
        && !tsrs_core::tspath::is_declaration_file_name(file_name)
        && (every_file || predicted_leaf(file_name))
}

#[inline]
fn stats() -> bool {
    // Relaxed: see `enable`.
    STATS.load(Ordering::Relaxed)
}

/// Runs `parse` (the host's `get_source_file` for a task of the loader) knowing whether the task is a root file of the
/// program (`root`, its include reason): only a file parsed for one can be a leaf, as a file parsed for an import or a
/// reference has a referrer, so only it gets a region (`wants_region`). In a monorepo app most files come from other
/// packages through imports (cal-diy: 2,643 of 3,550 files), and their regions would cost and free nothing.
pub(crate) fn parse_task<T>(root: bool, parse: impl FnOnce() -> T) -> T {
    // Relaxed: see `enable`.
    if !root || !REGIONS_ON.load(Ordering::Relaxed) {
        return parse();
    }
    let saved = ROOT_PARSE.replace(true);
    let file = parse();
    ROOT_PARSE.set(saved);
    file
}

/// Whether a file of this name and kind is parsed into a region of its own: a `region_candidate` parsed for a root file
/// of the program (`parse_task`).
pub(crate) fn wants_region(file_name: &str, script_kind: ScriptKind) -> bool {
    ROOT_PARSE.get() && region_candidate(file_name, script_kind)
}

// Text, AST and binder data take about 8 times the text (tsrs_project parsecache.rs measured 7.8x on the private
// monorepo); the region grows by a quarter when this is exceeded and is trimmed to what it used.
fn first_chunk(text_len: usize) -> usize {
    text_len.saturating_mul(10).saturating_add(4 << 10).min(64 << 20)
}

/// Parses `text` (just read) into a fresh region (`wants_region`). The text itself is kept outside it. The region is
/// trimmed right away: binding may come much later (single-threaded loads bind every file at the end), and the next
/// file's region is then carved right after this one's last used byte instead of after an unused tail and its partly
/// used page. Binding on the same thread right away (the parallel loader) grows the region into the space just given
/// back.
pub(crate) fn parse(
    opts: SourceFileParseOptions,
    text: String,
    script_kind: ScriptKind,
) -> P<SourceFile> {
    let region = Region::new_scratch_in_large_slabs(first_chunk(text.len()));
    let file = {
        let _scratch = region.enter_scratch();
        tsrs_parser::parse_source_file_keep_text(opts, text, script_kind)
    };
    region.trim();
    REGIONS.lock().unwrap().insert(file, region);
    file
}

/// `tsrs_binder::bind_source_file`, in the file's region if it has one (then trimmed to what parse and bind used, if
/// this thread carved it last).
pub(crate) fn bind(file: P<SourceFile>) {
    // Relaxed: see `bind_in_file_regions`.
    if !file.is_bound()
        && OWN_FILE_REGIONS.load(Ordering::Relaxed)
        && let Some(region) = Region::containing(file.addr())
    {
        {
            let _scope = region.enter();
            tsrs_binder::bind_source_file(file);
        }
        region.trim();
        return;
    }
    if file.is_bound() || !region_candidate(file.file_name(), file.script_kind.get()) {
        tsrs_binder::bind_source_file(file);
        return;
    }
    let Some(region) = REGIONS.lock().unwrap().get(&file).cloned() else {
        tsrs_binder::bind_source_file(file);
        return;
    };
    {
        let _scratch = region.enter_scratch();
        tsrs_binder::bind_source_file(file);
    }
    region.trim();
}

/// Marks the leaves among `program`'s files before its type-check pass (`SourceFile::set_check_leaf`) when its
/// `leaf_files` mode asks for it, and returns whether the pass frees them.
pub(crate) fn classify(program: &Program) -> bool {
    let mode = program.leaf_files;
    if mode == LeafMode::Off {
        return false;
    }
    // A second pass would read the freed files (their bind diagnostics and comment directives). The CLI runs one.
    // Relaxed: only the thread that runs the program's passes reads and writes it.
    assert!(
        mode != LeafMode::Free || !program.leaf_pass_started.swap(true, Ordering::Relaxed),
        "fileregions: a second type-check pass after leaf files were freed"
    );
    // The test reads the binder's output (the module symbol's exports, UMD globals, pattern ambient modules). The
    // CLI has bound every file by now (its bind diagnostics come first); creating the checkers would bind them next
    // anyway, in the same order.
    program.bind_source_files();
    let files = program.files;
    let regions = REGIONS.lock().unwrap();
    let referred = leaf_referred(program, |file| regions.contains_key(&file));
    let (mut leaves, mut leaf_bytes, mut checked) = (0, 0, 0);
    // `stats`: leaves the prediction missed (parsed into the thread arena, so not freed), and the nodes of both kinds.
    let (mut missed, mut missed_nodes, mut leaf_nodes) = (0, 0, 0);
    for &file in files {
        if !program.skip_type_checking(file, false) {
            checked += 1;
        }
        let Some(region) = regions.get(&file) else {
            if stats()
                && matches!(file.script_kind.get(), ScriptKind::TS | ScriptKind::TSX)
                && !referred.contains(&file)
                && adds_nothing(program, file)
            {
                missed += 1;
                missed_nodes += file.node_count.get();
            }
            continue;
        };
        if referred.contains(&file) || !adds_nothing(program, file) {
            continue;
        }
        file.set_check_leaf(true);
        leaves += 1;
        if stats() {
            leaf_bytes += region.used_bytes();
            leaf_nodes += file.node_count.get();
        }
    }
    #[expect(
        clippy::iter_over_hash_type,
        reason = "independent per region; no output depends on the order"
    )]
    for (file, region) in regions.iter() {
        // Never freed (also the regions of files parsed ahead and not used): their values live for the process, as
        // in a thread arena, so the list of the ones to drop (24 bytes per symbol table, the one heap-owning value
        // parse and bind make) goes now, before the check pass.
        if !file.is_check_leaf() {
            region.forget_drops();
        }
    }
    if stats() {
        let region_bytes = regions.values().map(Region::allocated_bytes).sum();
        let region_used = regions.values().map(Region::used_bytes).sum();
        for (counter, value) in [
            (&LEAVES, leaves),
            (&LEAF_BYTES, leaf_bytes),
            (&CHECKED_FILES, checked),
            (&REGION_FILES, regions.len()),
            (&REGION_BYTES, region_bytes),
            (&REGION_USED, region_used),
            (&MISSED, missed),
            (&MISSED_NODES, missed_nodes),
            (&LEAF_NODES, leaf_nodes),
        ] {
            // Relaxed: read on the main thread after the pass's threads joined.
            counter.store(value, Ordering::Relaxed);
        }
    }
    mode == LeafMode::Free
}

/// Computes the part of `classify` that reads only the loaded program (`leaf_referred`) ahead of the pass, while the
/// checker pool creates its checkers and assigns them files (checkerpool.rs `create_checkers`), so that it is not
/// one more serial step between them and the pass (vscode: 4-5 ms on the 64-vCPU runner, about 1% of the check time
/// at 32 checkers). Does nothing for a program that frees nothing.
pub(crate) fn prepare(program: &Program) {
    if program.leaf_files != LeafMode::Off {
        // Not under the lock: binding (`bind`) takes it, and creating a checker may bind.
        let with_region: FxHashSet<P<SourceFile>> =
            REGIONS.lock().unwrap().keys().copied().collect();
        leaf_referred(program, |file| with_region.contains(&file));
    }
}

/// `referred_files` of the files that can be leaves (those with a region; every file with `stats`, which also counts
/// the leaves the prediction missed), computed once per program. The program's files, include reasons and resolutions
/// do not change once it is loaded, and the regions are all made while it loads.
fn leaf_referred(
    program: &Program,
    has_region: impl Fn(P<SourceFile>) -> bool + Sync,
) -> &FxHashSet<P<SourceFile>> {
    program
        .leaf_referred
        .get_or_init(|| referred_files(program, |file| stats() || has_region(file)))
}

/// The files for which `wanted` holds that another file of the program refers to: every file with an include
/// reason other than being a root (an import, a `/// <reference path>`, a type reference directive, a lib
/// reference, a default lib, an automatic type directive), and every target of a resolved import, module
/// augmentation or type reference directive of another file. A file that refers only to itself is not
/// counted.
///
/// This runs inside the check time, before the pass. The resolutions (vscode: 110k) are read on the worker
/// pool, like the import graph of the checker assignment (checkerpool.rs `get_import_adjacency`), and a
/// resolved name is looked up by the name itself first (`files_by_name`): normalizing every name
/// (`get_source_file_for_resolved_module`) took 24 ms on one thread, and collecting every importer's names
/// before that another 3-5 ms on one thread.
fn referred_files(
    program: &Program,
    wanted: impl Fn(P<SourceFile>) -> bool + Sync,
) -> FxHashSet<P<SourceFile>> {
    let mut referred = FxHashSet::default();
    // builds a set; the order of insertion cannot be seen
    referred.extend(program.file_include_data.file_include_reasons.iter().filter_map(
        |(path, reasons)| {
            if reasons.iter().any(|r| !r.is_root_file()) {
                if let Some(&file) = program.files_by_path.get(path) {
                    if wanted(file) { Some(file) } else { None }
                } else {
                    None
                }
            } else {
                None
            }
        },
    ));
    let by_name = files_by_name(program);
    let target = |from: Option<P<SourceFile>>, name: &str| -> Option<P<SourceFile>> {
        let file = by_name
            .get(name)
            .copied()
            .or_else(|| program.get_source_file_for_resolved_module(name))?;
        (Some(file) != from && wanted(file)).then_some(file)
    };
    let modules = |(path, resolutions): (
        &tsrs_core::tspath::Path,
        &tsrs_module::ModeAwareCache<P<tsrs_module::ResolvedModule>>,
    )| {
        let from = program.files_by_path.get(path).copied();
        resolutions
            .values()
            .filter(|r| r.is_resolved())
            .filter_map(|r| target(from, r.resolved_file_name))
            .collect::<Vec<_>>()
    };
    let types = |(path, resolutions): (
        &tsrs_core::tspath::Path,
        &tsrs_module::ModeAwareCache<P<tsrs_module::ResolvedTypeReferenceDirective>>,
    )| {
        let from = program.files_by_path.get(path).copied();
        resolutions
            .values()
            .filter(|r| r.is_resolved())
            .filter_map(|r| target(from, r.resolved_file_name))
            .collect::<Vec<_>>()
    };
    let targets: Vec<Vec<P<SourceFile>>> = if program.single_threaded() {
        program
            .resolved_modules
            .iter()
            .map(modules)
            .chain(program.type_resolutions_in_file.iter().map(types))
            .collect()
    } else {
        use rayon::prelude::*;
        crate::program::worker_pool().install(|| {
            let mut targets: Vec<Vec<P<SourceFile>>> =
                program.resolved_modules.par_iter().map(modules).collect();
            targets.par_extend(program.type_resolutions_in_file.par_iter().map(types));
            targets
        })
    };
    referred.extend(targets.into_iter().flatten());
    referred
}

/// The program's files by name, for `referred_files`: only files that `files_by_path` maps their own path to, so that
/// finding a resolved name here gives the file `get_source_file_for_resolved_module` would (it looks the normalized
/// name up in `files_by_path`, and a file's path is its normalized name).
fn files_by_name(program: &Program) -> FxHashMap<&str, P<SourceFile>> {
    program
        .files
        .iter()
        .filter(|&&file| program.files_by_path.get(file.path()) == Some(&file))
        .map(|file| (file.file_name(), *file))
        .collect()
}

/// Whether nothing of `file` can be reached from another file that does not refer to it. It is a
/// type-checked, non-declaration TypeScript external module (its top-level declarations are module scoped;
/// JavaScript is excluded because `CommonJS` exports and expando assignments can declare globals), with no
/// module augmentation (including `declare global`), no ambient module, no pattern ambient module, no UMD
/// global (`export as namespace`), and no `// @ts-check` directive (read through the file by
/// `skip_type_checking`). And it exports no alias (`export ... from`, `export *`, `export =`, `export { local
/// }`, `export default <name>`): the checker's whole-program loops (`getAlternativeContainingModules`) ask
/// every module whether it re-exports a symbol, and a module whose exports are its own declarations answers
/// no for any symbol declared elsewhere, so they can skip it (`Checker::is_unreadable_check_leaf`).
fn adds_nothing(program: &Program, file: P<SourceFile>) -> bool {
    if program.skip_type_checking(file, false)
        || file.is_declaration_file.get()
        || !matches!(file.script_kind.get(), ScriptKind::TS | ScriptKind::TSX)
        || !ast::is_external_module(file)
        || !file.module_augmentations.get().is_empty()
        || !file.ambient_module_names.get().is_empty()
        || !file.pattern_ambient_modules.get().is_empty()
        || file.global_exports.get().is_some_and(|g| !g.is_empty())
        || file.check_js_directive.get().is_some()
    {
        return false;
    }
    let Some(symbol) = file.symbol() else {
        return false;
    };
    let Some(exports) = symbol.exports() else {
        return true;
    };
    exports.entries().iter().all(|&(name, s)| {
        name != ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS
            && name != ast::INTERNAL_SYMBOL_NAME_EXPORT_STAR
            && !s.flags().intersects(SymbolFlags::Alias | SymbolFlags::ExportStar)
    })
}

/// Panics if `file`'s tree was freed (a leaf after the type-check pass that freed it): for the single-file
/// diagnostic entry points, which read the tree, the bind and parse diagnostics and the comment directives. The CLI
/// never asks for one file's diagnostics after that pass, and the pass does not keep each file's settled diagnostics
/// (it returns them all together), so a caller that did would read freed memory; this makes it a clear error instead.
/// One atomic load for any other file.
/// Whether `file` was parsed into a region of its own: it may be freed once checked (the shared-graph seed must not
/// check it, sharedgraph.rs).
pub(crate) fn has_region(file: P<SourceFile>) -> bool {
    region_candidate(file.file_name(), file.script_kind.get())
        && REGIONS.lock().unwrap().contains_key(&file)
}

pub(crate) fn assert_not_freed(file: P<SourceFile>) {
    assert!(
        !file.is_check_leaf() || REGIONS.lock().unwrap().contains_key(&file),
        "fileregions: {} was freed after the type-check pass; its tree can no longer be read",
        file.file_name()
    );
}

/// After the type-check pass that freed leaves: gives back the pages of the last batch of freed regions.
pub(crate) fn pass_done() {
    tsrs_core::arena::flush_retired();
}

/// Frees `file`'s region (a leaf whose diagnostics the pass has collected). Called on the checker thread that
/// checked it; nothing reads the file's tree or binder output afterwards (`classify`).
pub(crate) fn free(file: P<SourceFile>) {
    let Some(region) = REGIONS.lock().unwrap().remove(&file) else {
        return;
    };
    if stats() {
        // Relaxed: counters read after the pass's threads joined.
        FREED.fetch_add(1, Ordering::Relaxed);
        FREED_BYTES.fetch_add(region.allocated_bytes(), Ordering::Relaxed);
    }
    region.retire_on_free();
    drop(region);
}

/// The `TSRS_FREE_LEAVES=stats` / `keep` line (for stderr), if asked for.
pub fn stats_report() -> Option<String> {
    if !stats() {
        return None;
    }
    let mb = |b: usize| b as f64 / (1 << 20) as f64;
    // Relaxed (all loads): read on the main thread after the pass's threads joined.
    let load = |c: &AtomicUsize| c.load(Ordering::Relaxed);
    let reserve = tsrs_core::ptr::reserve_stats()
        .map(|(_, high)| format!("; arena address space used {:.1} MB", mb(high)))
        .unwrap_or_default();
    Some(format!(
        "tsrs: leaf files: {} of {} checked files ({} more not predicted, {:.1}% of the leaves' nodes), {:.1} MB of the {:.1} MB used by {} file regions ({:.1} MB reserved); freed {} ({:.1} MB reserved){reserve}\n",
        load(&LEAVES),
        load(&CHECKED_FILES),
        load(&MISSED),
        100.0 * load(&MISSED_NODES) as f64
            / (load(&MISSED_NODES) + load(&LEAF_NODES)).max(1) as f64,
        mb(load(&LEAF_BYTES)),
        mb(load(&REGION_USED)),
        load(&REGION_FILES),
        mb(load(&REGION_BYTES)),
        load(&FREED),
        mb(load(&FREED_BYTES)),
    ))
}
