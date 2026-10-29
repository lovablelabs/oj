//! The process-wide address range behind compressed `P<T>` handles (`cfg(compressed_ptrs)`: on unix unless
//! `plain-ptrs` is enabled, see build.rs; notes/mem-pointer-compression.md).
//!
//! The first chunk request reserves `RESERVE` bytes of address space with no access, and every arena chunk of every
//! thread arena and region is carved from it and committed on hand-out, so one `base()` turns any handle into an
//! address: `base() + (handle << UNIT_SHIFT)`. The first `GRANULE` is never handed out, so handle 0 is never an
//! object (the `Option<P<T>>` niche). Released chunks (region slabs) are decommitted and their ranges reused.
//!
//! Linux: thread-arena chunks from 2 MiB up (`alloc_chunk` with `huge`) are whole, aligned 2 MiB blocks advised
//! with `MADV_HUGEPAGE`, so the kernel backs them with transparent huge pages where THP is `madvise` or `always`
//! (notes/linux-x86-round.md). Region slabs are released piecemeal and keep 4 KiB pages.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Handles count units of `1 << UNIT_SHIFT` bytes, so every `P` target is 8-aligned.
pub(crate) const UNIT_SHIFT: u32 = 3;
/// 2^32 units: 32 GiB.
pub const RESERVE: usize = 1 << (32 + UNIT_SHIFT);
/// Chunk sizes and offsets are multiples of this (a multiple of every supported page size).
pub const GRANULE: usize = 64 << 10;
/// Size and alignment of `huge` chunks: on Linux the transparent huge page of a 4 KiB-page kernel (x86-64, arm64).
///
/// A fault gets a huge page only if the aligned 2 MiB around it lies inside one advised read-write mapping, so a chunk
/// must not share a 2 MiB block with a neighbour. Elsewhere `GRANULE` (macOS has no huge pages for anonymous memory).
#[cfg(target_os = "linux")]
pub const HUGE_CHUNK: usize = 2 << 20;
#[cfg(not(target_os = "linux"))]
pub const HUGE_CHUNK: usize = GRANULE;

/// Keeps handle reconstruction to a constant-base address calculation.
///
/// Where the reservation lives: a fixed address, so `base()` is a constant and turning a handle into an
/// address costs an add, no load (a loaded base cost ~25% more instructions). 64 TiB + 4 GiB is clear of
/// every placement seen on the supported platforms: macOS keeps 4-448 GiB for the binary, the shared cache
/// and its malloc zones and places other mappings upward from 448 GiB; mimalloc hints from 2 to 30 TiB; Linux
/// puts PIE binaries near 85 TiB and maps top-down below the stack (near 128 TiB). It needs a 47-bit address
/// space (x86-64, arm64 with 48-bit VA).
///
/// The value is chosen for arm64 codegen: one `movz` materializes it (a single 16-bit chunk, `0x4001 << 32`),
/// and bit 32 overlaps the offset range (`handle << 3` < 2^35), so LLVM cannot turn the add into an `orr`;
/// `add x, base, w, uxtw #3` then folds the zero-extension and the shift of a 32-bit handle into the one
/// instruction. A base with disjoint bits (`0x4000_0000_0000`) gave `mov w, w` + `orr` per dereference of a
/// handle passed in a register.
pub(crate) const BASE_ADDR: usize = 0x4001_0000_0000;

static RESERVED: AtomicBool = AtomicBool::new(false);

/// The start of the reservation. A handle exists only after the first chunk, i.e. after the reservation.
#[inline]
pub fn base() -> *mut u8 {
    std::ptr::with_exposed_provenance_mut(BASE_ADDR)
}

pub struct Chunks {
    /// Offset of the first never-used byte.
    pub next: usize,
    /// Released ranges, offset -> length (coalesced).
    pub free: BTreeMap<usize, usize>,
    /// Bytes handed out and not released.
    pub live: usize,
    /// Highest `next` so far.
    pub peak: usize,
}

static CHUNKS: Mutex<Chunks> =
    Mutex::new(Chunks { next: GRANULE, free: BTreeMap::new(), live: 0, peak: GRANULE });

#[cold]
fn fail(what: &str) -> ! {
    let live = CHUNKS.try_lock().map_or(0, |c| c.live);
    eprintln!(
        "tsrs: {what} (compressed pointers: one {} GiB address range for every arena; {} MiB of it in use). \
         Build with the tsrs_core/plain-ptrs feature to lift the limit.",
        RESERVE >> 30,
        live >> 20
    );
    std::process::abort()
}

#[cfg(unix)]
fn reserve() {
    // SAFETY: a fresh anonymous mapping with no access at a hint; nothing else refers to it.
    let p = unsafe {
        libc::mmap(
            std::ptr::without_provenance_mut(BASE_ADDR),
            RESERVE,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        fail("could not reserve the arena address range");
    }
    if p.addr() != BASE_ADDR {
        // SAFETY: the mapping just made, unused.
        unsafe { libc::munmap(p, RESERVE) };
        fail(&format!(
            "the arena address range at {BASE_ADDR:#x} is taken (the system placed it at {:#x})",
            p.addr()
        ));
    }
    // `base()` derives every arena pointer from the address.
    let _ = p.expose_provenance();
}

#[cfg(unix)]
fn commit(p: *mut u8, len: usize, huge: bool) {
    // Before `mprotect`, so the range joins an advised read-write neighbour instead of splitting it again. A release
    // (`decommit`) replaces the mapping and drops the advice; the next commit of the range gives it again.
    #[cfg(target_os = "linux")]
    if huge {
        // SAFETY: `p .. p + len` lies inside the reservation. Advice only: it fails on kernels without THP and does
        // nothing where THP is `never`, and either way the chunk works with 4 KiB pages.
        unsafe { libc::madvise(p.cast(), len, libc::MADV_HUGEPAGE) };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = huge;
    // SAFETY: `p .. p + len` lies inside the reservation and is not in use.
    if unsafe { libc::mprotect(p.cast(), len, libc::PROT_READ | libc::PROT_WRITE) } != 0 {
        fail("could not commit arena memory");
    }
}

#[cfg(unix)]
fn decommit(p: *mut u8, len: usize) {
    // A fixed no-access mapping over the range drops its pages (macOS and Linux) and keeps the range reserved.
    // SAFETY: `p .. p + len` lies inside the reservation and nothing uses it any more.
    let q = unsafe {
        libc::mmap(
            p.cast(),
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    if q == libc::MAP_FAILED {
        fail("could not release arena memory");
    }
}

impl Chunks {
    /// Offset (a multiple of `align`) of a free range of `size` bytes (both multiples of `GRANULE`): first fit among
    /// the released ranges, else the end. What alignment skips stays free. `None` when the reservation is exhausted.
    pub fn take(&mut self, size: usize, align: usize) -> Option<usize> {
        let found = self.free.iter().find_map(|(&off, &len)| {
            let start = off.next_multiple_of(align);
            (start + size <= off + len).then_some((off, len, start))
        });
        let start = if let Some((off, len, start)) = found {
            self.free.remove(&off);
            if start > off {
                self.free.insert(off, start - off);
            }
            if off + len > start + size {
                self.free.insert(start + size, off + len - (start + size));
            }
            start
        } else {
            let start = self.next.next_multiple_of(align);
            if start + size > RESERVE {
                return None;
            }
            // No free range ends at `next` (`put` lowers `next` instead), so the gap is a range of its own.
            if start > self.next {
                self.free.insert(self.next, start - self.next);
            }
            self.next = start + size;
            self.peak = self.peak.max(self.next);
            start
        };
        self.live += size;
        Some(start)
    }

    /// Returns the range `off .. off + size` (from `take`), merged with its free neighbours.
    pub fn put(&mut self, mut off: usize, size: usize) {
        self.live -= size;
        let mut len = size;
        if let Some((&prev, &prev_len)) = self.free.range(..off).next_back()
            && prev + prev_len == off
        {
            self.free.remove(&prev);
            off = prev;
            len += prev_len;
        }
        if let Some(next_len) = self.free.remove(&(off + len)) {
            len += next_len;
        }
        if off + len == self.next {
            self.next = off;
        } else {
            self.free.insert(off, len);
        }
    }
}

/// A committed, zero-filled chunk of at least `size` bytes, `GRANULE`-aligned.
///
/// With `huge` (large thread-arena chunks,
/// which are never released): `HUGE_CHUNK`-aligned, a multiple of it, and on Linux advised for transparent huge pages;
/// callers round `size` to `HUGE_CHUNK` themselves so that they use the whole chunk.
pub fn alloc_chunk(size: usize, huge: bool) -> *mut u8 {
    let align = if huge { HUGE_CHUNK } else { GRANULE };
    let size = size.next_multiple_of(align);
    let mut c = CHUNKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !RESERVED.load(Ordering::Relaxed) {
        reserve();
        RESERVED.store(true, Ordering::Relaxed);
    }
    let base = base();
    let Some(off) = c.take(size, align) else {
        drop(c);
        fail("the arena address range is exhausted");
    };
    drop(c);
    // SAFETY: inside the reservation.
    let p = unsafe { base.add(off) };
    commit(p, size, huge);
    p
}

/// Gives back a chunk from `alloc_chunk` (same `size`); its memory is released and its range reused.
///
/// # Safety
/// Nothing may use the chunk afterwards.
pub unsafe fn release_chunk(p: *mut u8, size: usize) {
    let size = size.next_multiple_of(GRANULE);
    decommit(p, size);
    let off = p.addr() - base().addr();
    CHUNKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).put(off, size);
}

/// The system page size (16 KiB on Apple silicon, 4 KiB on most Linux machines).
#[cfg(unix)]
pub fn page_size() -> usize {
    static PAGE_SIZE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *PAGE_SIZE.get_or_init(|| {
        // SAFETY: sysconf reads a system constant.
        usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
            .ok()
            .filter(|p| p.is_power_of_two())
            .unwrap_or(4096)
    })
}

/// Gives the memory of the whole pages inside `p .. p + len` (part of a chunk from `alloc_chunk`) back to the
/// system without returning the range: it is never handed out again, so no later object gets the address of
/// one that was there (a stale key in a table keyed by address stays stale). For a freed file region whose
/// neighbours in its slab are alive (`arena::Region::retire_on_free`). Linux: `MADV_DONTNEED` (no mapping
/// split, so no growth in the number of mappings; a stray read sees zeros). Elsewhere: a no-access mapping
/// over the pages, as `decommit` (a stray access faults), which also takes them out of the macOS footprint at
/// once (`MADV_FREE` only marks them reclaimable).
///
/// Bound: a discarded range stays taken, so a run uses as much of the reservation as if the memory had never
/// been freed, which is what the batch compiler did before. The retired file regions of a `--noEmit` check
/// hold their chunks' capacity (vscode: 408 MB for 2,337 leaves; 2.0 GB of the reservation in use at the peak
/// with 4 checkers, with or without freeing). File regions take about a third more reservation than the bytes
/// they use (unused chunk tails: vscode 1,050 MB for 794 MB), so about 0.3 GiB more on the 38k-file codebase
/// (27.5k TypeScript files, 954 MiB of parse and bind output), whose worst measured peak is 11.4 GiB of the
/// 32 GiB (notes/mem-free-leaf-files.md).
///
/// # Safety
/// Nothing may use the memory afterwards.
#[cfg(unix)]
pub(crate) unsafe fn discard(p: *mut u8, len: usize) {
    let page = page_size();
    let start = p.addr().next_multiple_of(page);
    let end = (p.addr() + len) & !(page - 1);
    if end <= start {
        return;
    }
    let q = p.with_addr(start);
    #[cfg(target_os = "linux")]
    // SAFETY: whole pages inside the reservation that nothing uses any more (this function's contract). Advice
    // only: if it fails the pages stay resident and nothing else changes.
    unsafe {
        libc::madvise(q.cast(), end - start, libc::MADV_DONTNEED)
    };
    #[cfg(not(target_os = "linux"))]
    decommit(q, end - start);
}

/// Bytes of the reservation handed out as chunks and not released.
pub(crate) fn reserved_in_use() -> usize {
    CHUNKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).live
}

/// How far into the reservation chunks have ever been carved (released ranges are reused before it grows).
pub(crate) fn reserved_high_water() -> usize {
    CHUNKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).peak
}
