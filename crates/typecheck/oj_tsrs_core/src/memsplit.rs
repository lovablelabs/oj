//! Reports how resident memory is divided between arenas and the allocator.
//!
//! `TSRS_MEM_SPLIT=1` (debug stat, any build): splits the resident memory at three points of a run into the thread arenas' used bytes, the resident but never used parts of their chunks, the allocator heap's live blocks and the rest it keeps resident, thread stacks and file-backed pages, and prints one block per point on stderr (notes/mem-linux-residency-32.md).
//!
//!
//! The points: `parse end` (the program is parsed and bound, before checker creation), `check end` (every
//! checker of the type-check pass has finished its files and its thread is still alive: the moment memory
//! peaks) and `exit`. `TSRS_MEM_SPLIT=purge` also makes the allocator purge its freed memory after `check
//! end` and prints the split again. Linux reads `/proc/self/smaps` and `/proc/self/status`; elsewhere
//! only the arena and heap lines print.

use std::fmt::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::arena::Arena;

fn mode() -> u8 {
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("TSRS_MEM_SPLIT").as_deref() {
        Ok("1") => 1,
        Ok("purge") => 2,
        _ => 0,
    })
}

/// Whether `TSRS_MEM_SPLIT` is `1` or `purge`. Read once.
pub fn enabled() -> bool {
    mode() != 0
}

/// Distinguishes reusable allocator pages from live heap storage.
///
/// `TSRS_MEM_SPLIT=purge`: after the `check end` report, the allocator purges every arena's freed memory
/// (`mi_collect(true)`) and the split is printed again, which shows how much of the heap's resident rest is
/// freed memory not yet given back.
pub fn purge_after_check() -> bool {
    mode() == 2
}

struct ArenaRef(&'static Arena);
// SAFETY: `report` reads an arena only while the threads that allocate are parked (the barrier at `check end`, a
// joined work group, an idle pool), which orders the reads after the owner's writes.
unsafe impl Send for ArenaRef {}

static ARENAS: Mutex<Vec<ArenaRef>> = Mutex::new(Vec::new());

/// Records a new thread arena when the stat is on.
pub(crate) fn register_arena(arena: &'static Arena) {
    if enabled() {
        ARENAS.lock().unwrap().push(ArenaRef(arena));
    }
}

/// The allocator heap's own view (`mi_heap_visit_blocks` in the CLI): pages, bytes of their initialized blocks, bytes
/// of live blocks.
#[derive(Clone, Copy, Default)]
pub struct HeapStats {
    pub pages: usize,
    pub capacity: usize,
    pub used: usize,
    /// Resident bytes of the pages' areas (`mincore`), header and partly initialized tail included.
    pub resident: usize,
}

static HEAP_STATS: OnceLock<fn() -> HeapStats> = OnceLock::new();
static HEAP_COLLECT: OnceLock<fn(bool)> = OnceLock::new();

/// Installs the heap walker and the allocator's collect (`mi_collect`), from the binary that owns the global
/// allocator.
pub fn set_heap_hooks(stats: fn() -> HeapStats, collect: fn(bool)) {
    let _ = HEAP_STATS.set(stats);
    let _ = HEAP_COLLECT.set(collect);
}

/// The allocator's collect on the calling thread (`force` also purges every arena's freed memory), if installed.
pub fn heap_collect(force: bool) {
    if let Some(f) = HEAP_COLLECT.get() {
        f(force);
    }
}

/// Resident bytes of the stacks of the threads parked at the `check end` point, measured by each thread itself
/// (`note_own_stack`).
static CHECKER_STACKS: AtomicUsize = AtomicUsize::new(0);
static CHECKER_STACK_THREADS: AtomicUsize = AtomicUsize::new(0);

/// Adds the calling thread's resident stack pages to the `check end` report (called by each checker thread before the
/// report).
pub fn note_own_stack() {
    if let Some(bytes) = own_stack_resident() {
        // Relaxed (both): summed before a barrier that orders them before the report.
        CHECKER_STACKS.fetch_add(bytes, Ordering::Relaxed);
        CHECKER_STACK_THREADS.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(target_os = "linux")]
fn own_stack_resident() -> Option<usize> {
    // SAFETY: pthread_getattr_np fills the attribute object for the calling thread; it is destroyed after use.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &raw mut attr) != 0 {
            return None;
        }
        let mut addr: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let ok = libc::pthread_attr_getstack(&raw const attr, &raw mut addr, &raw mut size) == 0;
        libc::pthread_attr_destroy(&raw mut attr);
        ok.then(|| resident(addr.addr(), size))
    }
}

#[cfg(not(target_os = "linux"))]
fn own_stack_resident() -> Option<usize> {
    None
}

/// Resident bytes of the whole pages inside `start .. start + len` (`mincore`).
#[cfg(unix)]
pub fn resident(start: usize, len: usize) -> usize {
    // SAFETY: sysconf reads a system constant.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096);
    let s = start.next_multiple_of(page);
    let e = (start + len) & !(page - 1);
    if e <= s {
        return 0;
    }
    let mut vec = vec![0u8; (e - s) / page];
    // SAFETY: mincore only reads the page tables of the range and writes one byte per page into `vec`.
    let rc = unsafe {
        libc::mincore(std::ptr::without_provenance_mut(s), e - s, vec.as_mut_ptr().cast())
    };
    if rc != 0 {
        return 0;
    }
    vec.iter().filter(|&&b| b & 1 != 0).count() * page
}

#[cfg(not(unix))]
pub fn resident(_start: usize, _len: usize) -> usize {
    0
}

const MIB: f64 = (1 << 20) as f64;

fn mib(b: usize) -> String {
    format!("{:.1}", b as f64 / MIB)
}

#[derive(Default)]
struct ArenaTotals {
    count: usize,
    pub capacity: usize,
    pub used: usize,
    /// Resident bytes below the finger of the current chunk (never handed out).
    tail_resident: usize,
    /// Resident bytes below the finger of retired chunks.
    retired_resident: usize,
    /// Arenas whose current chunk is a huge-page chunk (2 MiB blocks).
    huge_current: usize,
}

/// Prints the split for point `what` on stderr.
pub fn report(what: &str) {
    if !enabled() {
        return;
    }
    let mut out = format!("tsrs mem split: {what}\n");
    let mut arenas = ArenaTotals::default();
    for a in ARENAS.lock().unwrap().iter() {
        let r = a.0.residency();
        arenas.count += 1;
        arenas.capacity += r.capacity;
        arenas.used += r.used;
        arenas.tail_resident += r.current_unused.map_or(0, |(s, l)| resident(s, l));
        arenas.retired_resident +=
            r.retired_unused.iter().map(|&(s, l)| resident(s, l)).sum::<usize>();
        arenas.huge_current += usize::from(r.current_huge);
    }
    let _ = writeln!(
        out,
        "  thread arenas: {} arenas, {} MiB capacity, {} MiB used, unused resident {} MiB in current chunks + {} MiB in retired chunks, {} huge current chunks",
        arenas.count,
        mib(arenas.capacity),
        mib(arenas.used),
        mib(arenas.tail_resident),
        mib(arenas.retired_resident),
        arenas.huge_current
    );
    #[cfg(compressed_ptrs)]
    let _ =
        writeln!(out, "  reservation: {} MiB in chunks", mib(crate::reserve::reserved_in_use()));
    let heap = HEAP_STATS.get().map(|f| f());
    if let Some(h) = heap {
        let _ = writeln!(
            out,
            "  heap (allocator walk): {} pages, {} MiB initialized blocks, {} MiB live blocks, {} MiB resident in the pages' areas",
            h.pages,
            mib(h.capacity),
            mib(h.used),
            mib(h.resident)
        );
    }
    // Relaxed (both): the checker threads added to them before the barrier this report runs behind.
    let stacks = CHECKER_STACKS.swap(0, Ordering::Relaxed);
    let stack_threads = CHECKER_STACK_THREADS.swap(0, Ordering::Relaxed);
    if stack_threads > 0 {
        let _ = writeln!(
            out,
            "  checker stacks (own mincore): {stack_threads} threads, {} MiB resident",
            mib(stacks)
        );
    }
    #[cfg(target_os = "linux")]
    linux::append(&mut out, &arenas, heap);
    eprint!("{out}");
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{ArenaTotals, HeapStats, mib};
    use std::fmt::Write;

    #[derive(Default)]
    struct Bucket {
        vmas: usize,
        rss: usize,
        huge: usize,
    }

    fn kb(line: &str) -> usize {
        line.split_whitespace().nth(1).and_then(|v| v.parse::<usize>().ok()).unwrap_or(0) << 10
    }

    pub(super) fn append(out: &mut String, arenas: &ArenaTotals, heap: Option<HeapStats>) {
        let status = crate::utf8::read_to_string("/proc/self/status").unwrap_or_default();
        let field = |name: &str| status.lines().find(|l| l.starts_with(name)).map_or(0, kb);
        let threads = status
            .lines()
            .find(|l| l.starts_with("Threads:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("?");
        let _ = writeln!(
            out,
            "  status: VmRSS {} MiB, VmHWM {} MiB, RssAnon {} MiB, RssFile {} MiB, threads {threads}",
            mib(field("VmRSS:")),
            mib(field("VmHWM:")),
            mib(field("RssAnon:")),
            mib(field("RssFile:"))
        );
        let smaps = crate::utf8::read_to_string("/proc/self/smaps").unwrap_or_default();
        #[cfg(compressed_ptrs)]
        let arena_range =
            (crate::reserve::BASE_ADDR, crate::reserve::BASE_ADDR + crate::reserve::RESERVE);
        #[cfg(not(compressed_ptrs))]
        let arena_range = (0usize, 0usize);
        let names =
            ["arena reservation", "thread stacks", "heap and other anonymous", "file-backed"];
        let mut buckets: [Bucket; 4] = Default::default();
        let mut stacks_by_size: std::collections::BTreeMap<usize, (usize, usize)> =
            std::collections::BTreeMap::new();
        let mut cur: Option<(usize, usize)> = None;
        for line in smaps.lines() {
            let mut words = line.split_whitespace();
            let first = words.next().unwrap_or("");
            if let Some((a, b)) = first
                .split_once('-')
                .filter(|(a, _)| a.chars().all(|c| c.is_ascii_hexdigit()) && !a.is_empty())
            {
                let (Ok(start), Ok(end)) =
                    (usize::from_str_radix(a, 16), usize::from_str_radix(b, 16))
                else {
                    continue;
                };
                let perms = words.next().unwrap_or("");
                let path = words.nth(3).unwrap_or("");
                let size = end - start;
                // Thread stacks by size: the checker threads' and the CLI thread's 512 MiB, the parse workers' 256 MiB
                // (nothing else maps anonymous memory of that size).
                let class = if start >= arena_range.0 && end <= arena_range.1 {
                    0
                } else if path == "[stack]"
                    || (path.is_empty()
                        && perms.starts_with("rw")
                        && (200 << 20..=600 << 20).contains(&size))
                {
                    1
                } else if path.starts_with('/') {
                    3
                } else {
                    2
                };
                buckets[class].vmas += 1;
                cur = Some((class, size));
            } else if let Some((class, size)) = cur {
                if first == "Rss:" {
                    let rss = kb(line);
                    buckets[class].rss += rss;
                    if class == 1 {
                        let e = stacks_by_size.entry(size >> 20).or_default();
                        e.0 += 1;
                        e.1 += rss;
                    }
                } else if first == "AnonHugePages:" {
                    buckets[class].huge += kb(line);
                }
            }
        }
        let _ = writeln!(out, "  smaps: bucket, mappings, Rss MiB, of which AnonHugePages MiB");
        let mut rss_total = 0;
        for (name, b) in names.iter().zip(&buckets) {
            rss_total += b.rss;
            let _ = writeln!(out, "    {name}: {}, {}, {}", b.vmas, mib(b.rss), mib(b.huge));
        }
        let _ = writeln!(out, "    total: {} MiB", mib(rss_total));
        for (size, (n, rss)) in &stacks_by_size {
            let _ = writeln!(out, "    stacks of {size} MiB: {n}, {} MiB resident", mib(*rss));
        }
        let arena_slack = arenas.tail_resident + arenas.retired_resident;
        let arena_other = buckets[0].rss as f64 - (arenas.used + arena_slack) as f64;
        let _ = writeln!(
            out,
            "  split MiB: arena used {} + arena unused resident {} + arena other (regions, untouched used) {:.1} | heap live {} + heap retained {} (in pages {}, outside pages {}) | stacks {} | file {} | = {}",
            mib(arenas.used),
            mib(arena_slack),
            arena_other / super::MIB,
            heap.map_or("?".into(), |h| mib(h.used)),
            heap.map_or("?".into(), |h| mib(buckets[2].rss.saturating_sub(h.used))),
            heap.map_or("?".into(), |h| mib(h.resident.saturating_sub(h.used))),
            heap.map_or("?".into(), |h| mib(buckets[2].rss.saturating_sub(h.resident))),
            mib(buckets[1].rss),
            mib(buckets[3].rss),
            mib(rss_total)
        );
    }
}

/// Reads the memory usage that the operating system charges to the process.
///
/// tsrs-only (`--maxMemory`, notes/mem-recycle-checkers.md): the process's memory in bytes as the system
/// accounts it for limits: the physical footprint on macOS (what `/usr/bin/time -l` reports as peak memory
/// footprint), the resident set on Linux (`/proc/self/statm`).
///
/// Re-read at most every 5 ms; callers between two checked files share the value.
pub fn process_memory() -> usize {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    static LAST_NS: AtomicU64 = AtomicU64::new(0);
    static VALUE: AtomicUsize = AtomicUsize::new(0);
    let now = START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64;
    // Relaxed (both): a cached reading; a stale or doubly refreshed value only moves a heuristic by 5 ms.
    let last = LAST_NS.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < 5_000_000 {
        return VALUE.load(Ordering::Relaxed);
    }
    let v = read_process_memory();
    // Relaxed (both): these atomics publish only a heuristic sample and its refresh time; readers may see
    // a stale value with a newer timestamp, since neither synchronizes access to other memory.
    VALUE.store(v, Ordering::Relaxed);
    LAST_NS.store(now.max(1), Ordering::Relaxed);
    v
}

#[cfg(target_os = "macos")]
fn read_process_memory() -> usize {
    unsafe extern "C" {
        #[link_name = "mach_task_self_"]
        static MACH_TASK_SELF_: u32;
        fn task_info(
            target_task: u32,
            flavor: i32,
            task_info_out: *mut u32,
            count: *mut u32,
        ) -> i32;
    }
    const TASK_VM_INFO: i32 = 22;
    // `task_vm_info_data_t`: `phys_footprint` is the u64 at byte 144 (revision 1 and later).
    let mut info = [0u32; 128];
    let mut count = info.len() as u32;
    // SAFETY: `info` has room for `count` naturals; the kernel writes at most that many and updates `count`.
    let kr = unsafe { task_info(MACH_TASK_SELF_, TASK_VM_INFO, info.as_mut_ptr(), &raw mut count) };
    if kr != 0 || count < 38 {
        return 0;
    }
    (u64::from(info[36]) | (u64::from(info[37]) << 32)) as usize
}

#[cfg(target_os = "linux")]
fn read_process_memory() -> usize {
    let statm = crate::utf8::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: usize = statm.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
    // SAFETY: sysconf has no preconditions.
    pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read_process_memory() -> usize {
    0
}
