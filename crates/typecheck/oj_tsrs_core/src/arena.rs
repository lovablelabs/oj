//! The per-thread leak arena behind `P<T>` (see `ptr`): a bump allocator (downward, chunks doubling in size like
//! bumpalo's) with two ways to give memory back that is provably dead:
//!
//! - **Free lists** (`free`, `free_slice`, `P::new_recycled`, `alloc_slice_recycled`): a block whose owner proved
//!   it unreachable goes onto a per-thread list for its size class (8-byte steps up to `MAX_FREE_SIZE`), and the
//!   recycling allocation sites take blocks from there first. Ids of arena objects never depend on addresses, so
//!   reuse changes no output.
//! - **Checkpoints** (`checkpoint`, `rewind`): a speculative parse records the bump position and, when it is rolled
//!   back, moves it back, discarding everything allocated since. This only happens when nothing could have kept a
//!   pointer to that memory: same chunk, and no free or `pin` on this thread in between (`pin` is called by code
//!   that stores freshly allocated data in a structure that outlives the speculation, e.g. a scanner cache).
//!
//! `TSRS_ARENA_POISON=1` never reuses memory: freed blocks and rewound ranges are filled with `POISON`
//! so later uses of stale pointers can be detected.
//!
//! **Regions** (docs/LSP.md "Memory plan for a long-lived server"): a `Region` is an arena of its own that can be
//! freed as a whole. `Region::enter` makes it the current thread's allocation target until the returned scope is
//! dropped (scopes nest); without a scope every allocation goes to the thread's own leak arena, exactly as before.
//! A region owns its chunks, its free lists and a drop list of the values allocated in it whose type needs drop (heap
//! vectors and maps inside links and tables), so freeing it (dropping the last `Region` handle) also releases the
//! heap memory those values own. Freeing is safe only if nothing that outlives the region points into it; the
//! language server ties regions to the objects Go's GC would free together (a parsed file version, a checker, a
//! program).

use std::alloc::Layout;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};

const FIRST_CHUNK: usize = 1 << 20;
const CHUNK_ALIGN: usize = 16;
const PAGE: usize = 4096;
/// Linux with compressed pointers: thread-arena chunks of at least this size (all but a thread's first chunk) are
/// whole 2 MiB blocks backed by transparent huge pages (`reserve::alloc_chunk`), and their sizes are rounded to it,
/// since an unused tail would be resident with the huge page it shares. A thread that allocates less than its first
/// chunk keeps 4 KiB pages. `None` elsewhere.
#[cfg(all(compressed_ptrs, target_os = "linux"))]
const HUGE_THREAD_CHUNK: Option<usize> = Some(crate::reserve::HUGE_CHUNK);
#[cfg(not(all(compressed_ptrs, target_os = "linux")))]
const HUGE_THREAD_CHUNK: Option<usize> = None;
/// Largest chunk a thread arena grows to in compressed mode (larger single allocations still get their own size).
#[cfg(compressed_ptrs)]
const MAX_CHUNK: usize = 64 << 20;
/// 32-bit targets (wasm32) cap the doubling too: a doubled 1 GiB chunk would waste half of a 4 GiB address space.
#[cfg(all(not(compressed_ptrs), target_pointer_width = "32"))]
const MAX_CHUNK: usize = 64 << 20;

/// Largest block size kept on a free list; classes are `size / 8`.
const MAX_FREE_SIZE: usize = 512;
const CLASSES: usize = MAX_FREE_SIZE / 8 + 1;

/// Fill byte for freed and rewound memory in poison mode (and in debug builds).
const POISON: u8 = 0xA5;

pub struct Arena {
    /// Bump finger in the current chunk (allocation moves it down towards `start`; in a region, up towards `end`).
    ptr: Cell<*mut u8>,
    /// Regions bump upwards: the allocator that hands out a chunk writes its first word (mimalloc's free list), so
    /// the start of every chunk is resident anyway, and the unused part of a region's last chunk (most regions are
    /// small, one per parsed file) should be the untouched end.
    up: bool,
    start: Cell<*mut u8>,
    end: Cell<*mut u8>,
    /// Older chunks: (start, end, finger when the chunk was retired).
    retired: RefCell<Vec<(usize, usize, usize)>>,
    capacity: Cell<usize>,
    /// Bumped by every free and every `pin`; a rewind is skipped when it changed since the checkpoint.
    epoch: Cell<u64>,
    free: [Cell<*mut u8>; CLASSES],
    /// Regions only (`None` for a thread's own arena): the region, for the chunk registry.
    region: Option<Weak<RegionInner>>,
    /// Regions only: values that need drop, dropped when the region is freed.
    drops: RefCell<Vec<DropEntry>>,
    /// Regions only: the slab each chunk was carved from, in chunk order (retired chunks, then the current one).
    slabs: RefCell<Vec<*const Slab>>,
    /// Regions only: the chunks are in the registry (`Region::containing`); scratch regions are not.
    registered: bool,
    /// Regions only: chunks are carved from the thread's large slabs (`Region::new_scratch_in_large_slabs`).
    large_slabs: bool,
}

pub(crate) struct Residency {
    pub(crate) capacity: usize,
    pub(crate) used: usize,
    pub(crate) current_unused: Option<(usize, usize)>,
    pub(crate) retired_unused: Vec<(usize, usize)>,
    pub(crate) current_huge: bool,
}

struct DropEntry {
    ptr: *mut u8,
    len: usize,
    drop: unsafe fn(*mut u8, usize),
}

/// # Safety
/// `ptr` holds `len` live `T`s that are never used again (a region's drop list, run once when it is freed).
unsafe fn drop_slice<T>(ptr: *mut u8, len: usize) {
    // SAFETY: this function's contract.
    unsafe { std::ptr::drop_in_place(std::ptr::slice_from_raw_parts_mut(ptr.cast::<T>(), len)) };
}

/// The bump position of the current thread's arena (`checkpoint`).
#[derive(Clone, Copy)]
pub struct Checkpoint {
    ptr: *mut u8,
    start: *mut u8,
    epoch: u64,
    drops: usize,
}

impl Arena {
    pub(crate) fn new() -> Arena {
        Arena::with_first_chunk(FIRST_CHUNK, None, false, false)
    }

    fn with_first_chunk(
        first_chunk: usize,
        region: Option<Weak<RegionInner>>,
        registered: bool,
        large_slabs: bool,
    ) -> Arena {
        let a = Arena {
            ptr: Cell::new(std::ptr::null_mut()),
            up: region.is_some(),
            start: Cell::new(std::ptr::null_mut()),
            end: Cell::new(std::ptr::null_mut()),
            retired: RefCell::new(Vec::new()),
            capacity: Cell::new(0),
            epoch: Cell::new(0),
            free: [const { Cell::new(std::ptr::null_mut()) }; CLASSES],
            region,
            drops: RefCell::new(Vec::new()),
            slabs: RefCell::new(Vec::new()),
            registered,
            large_slabs,
        };
        a.new_chunk(first_chunk);
        a
    }

    #[inline]
    fn is_region(&self) -> bool {
        self.region.is_some()
    }

    /// Records `len` values of type `T` at `ptr` (just allocated in this arena) for dropping when the region is freed.
    /// Does nothing in a thread's own arena (values there are never dropped).
    #[inline]
    pub(crate) fn track_drop<T>(&self, ptr: *mut T, len: usize) {
        if std::mem::needs_drop::<T>() && self.is_region() && len != 0 {
            self.drops.borrow_mut().push(DropEntry { ptr: ptr.cast(), len, drop: drop_slice::<T> });
        }
    }

    #[inline]
    pub(crate) fn alloc_layout(&self, layout: Layout) -> NonNull<u8> {
        if self.up {
            return self.alloc_layout_up(layout);
        }
        let ptr = self.ptr.get();
        let new = ptr.addr().wrapping_sub(layout.size()) & !(layout.align() - 1);
        if new >= self.start.get().addr() && new <= ptr.addr() {
            let p = ptr.with_addr(new);
            self.ptr.set(p);
            // SAFETY: `new` lies inside the current chunk (at or above `start`), which is never null.
            return unsafe { NonNull::new_unchecked(p) };
        }
        self.alloc_layout_slow(layout)
    }

    #[inline]
    fn alloc_layout_up(&self, layout: Layout) -> NonNull<u8> {
        let ptr = self.ptr.get();
        let aligned = (ptr.addr() + (layout.align() - 1)) & !(layout.align() - 1);
        let next = aligned.wrapping_add(layout.size());
        if next <= self.end.get().addr() && next >= aligned {
            self.ptr.set(ptr.with_addr(next));
            // SAFETY: `aligned` lies inside the current chunk, which is never null.
            return unsafe { NonNull::new_unchecked(ptr.with_addr(aligned)) };
        }
        self.alloc_layout_slow(layout)
    }

    #[cold]
    #[inline(never)]
    fn alloc_layout_slow(&self, layout: Layout) -> NonNull<u8> {
        let prev = self.end.get().addr() - self.start.get().addr();
        let need =
            layout.size().checked_add(layout.align()).expect("arena allocation size overflow");
        // A region grows by a quarter of what it has (at least 4 KiB): many regions are small (one per parsed file),
        // and the unused tail of a doubled chunk would dominate their footprint.
        // Compressed pointers: thread-arena chunks stop doubling at `MAX_CHUNK`, since every chunk takes its size out
        // of the fixed reservation, touched or not.
        let next = if self.is_region() {
            (self.capacity.get() / 4).max(PAGE)
        } else {
            #[cfg(any(compressed_ptrs, target_pointer_width = "32"))]
            {
                (prev * 2).min(MAX_CHUNK)
            }
            #[cfg(not(any(compressed_ptrs, target_pointer_width = "32")))]
            {
                prev * 2
            }
        };
        self.new_chunk(next.max(need));
        if self.up {
            return self.alloc_layout_up(layout);
        }
        let ptr = self.ptr.get();
        let new = (ptr.addr() - layout.size()) & !(layout.align() - 1);
        debug_assert!(new >= self.start.get().addr());
        let p = ptr.with_addr(new);
        self.ptr.set(p);
        // SAFETY: inside the fresh chunk.
        unsafe { NonNull::new_unchecked(p) }
    }

    fn new_chunk(&self, size: usize) {
        if self.up {
            let size = size.next_multiple_of(CHUNK_ALIGN);
            let (base, slab) = slab_carve(size, self.large_slabs);
            self.slabs.borrow_mut().push(slab);
            self.new_chunk_at(base, size);
        } else {
            let huge = HUGE_THREAD_CHUNK.filter(|&h| size >= h);
            let size = size.next_multiple_of(huge.unwrap_or(PAGE));
            let layout = Layout::from_size_align(size, CHUNK_ALIGN).expect("arena chunk layout");
            self.new_chunk_at(os_chunk(layout, huge.is_some()), size);
        }
    }

    fn new_chunk_at(&self, base: *mut u8, size: usize) {
        // Blocks given back by address (`free_block`) are written through this provenance.
        let _ = base.expose_provenance();
        if !self.start.get().is_null() {
            self.retired.borrow_mut().push((
                self.start.get().addr(),
                self.end.get().addr(),
                self.ptr.get().addr(),
            ));
        }
        self.start.set(base);
        // SAFETY: one past the end of the allocation.
        self.end.set(unsafe { base.add(size) });
        self.ptr.set(if self.up { base } else { self.end.get() });
        self.capacity.set(self.capacity.get() + size);
        if let (Some(region), true) = (&self.region, self.registered) {
            REGISTRY
                .write()
                .unwrap()
                .insert(reg_key(base.addr()), (reg_key(base.addr() + size), Weak::clone(region)));
        }
    }

    /// Region only: gives the unused end of the current chunk back to the slab it was carved from, if it was the
    /// last carve on this thread (a file region right after parse and bind).
    fn trim(&self) {
        let (start, end, ptr) = (self.start.get(), self.end.get(), self.ptr.get());
        let new_end = ptr.addr().next_multiple_of(CHUNK_ALIGN).max(start.addr() + CHUNK_ALIGN);
        let Some(&slab) = self.slabs.borrow().last() else {
            return;
        };
        if new_end >= end.addr() || !slab_trim(slab, end.addr(), new_end) {
            return;
        }
        self.end.set(end.with_addr(new_end));
        self.capacity.set(self.capacity.get() - (end.addr() - new_end));
        if let (Some(region), true) = (&self.region, self.registered) {
            REGISTRY
                .write()
                .unwrap()
                .insert(reg_key(start.addr()), (reg_key(new_end), Weak::clone(region)));
        }
    }

    /// Every chunk, (start, size).
    fn chunks(&self) -> Vec<(usize, usize)> {
        let mut out =
            vec![(self.start.get().addr(), self.end.get().addr() - self.start.get().addr())];
        out.extend(self.retired.borrow().iter().map(|&(start, end, _)| (start, end - start)));
        out
    }

    /// Whether `addr` lies in one of this arena's chunks (debug checks).
    #[cfg(debug_assertions)]
    fn owns(&self, addr: usize) -> bool {
        self.chunks().iter().any(|&(start, size)| addr >= start && addr < start + size)
    }

    /// `value` in a block of `layout` (at least `T`'s size and alignment).
    #[inline]
    #[expect(
        clippy::mut_from_ref,
        reason = "a bump allocator: each call returns a fresh block nothing else points to (bumpalo's alloc has this signature)"
    )]
    pub(crate) fn alloc_with<T>(&self, layout: Layout, value: T) -> &mut T {
        debug_assert!(
            layout.size() >= std::mem::size_of::<T>()
                && layout.align() >= std::mem::align_of::<T>()
        );
        let p = self.alloc_layout(layout).cast::<T>();
        // SAFETY: fresh, aligned, exclusively owned memory for one `T`.
        unsafe {
            p.as_ptr().write(value);
            &mut *p.as_ptr()
        }
    }

    #[inline]
    #[expect(clippy::mut_from_ref, reason = "a fresh block, as in alloc_with")]
    pub(crate) fn alloc_slice_copy<T: Copy>(&self, src: &[T]) -> &mut [T] {
        let p = self.alloc_layout(Layout::for_value(src)).cast::<T>();
        // SAFETY: fresh memory for `src.len()` items; `T: Copy`.
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), p.as_ptr(), src.len());
            std::slice::from_raw_parts_mut(p.as_ptr(), src.len())
        }
    }

    #[inline]
    #[expect(clippy::mut_from_ref, reason = "a fresh block, as in alloc_with")]
    pub(crate) fn alloc_vec<T>(&self, items: Vec<T>) -> &mut [T] {
        let len = items.len();
        let p = self.alloc_layout(Layout::array::<T>(len).expect("arena slice layout")).cast::<T>();
        let mut items = std::mem::ManuallyDrop::new(items);
        // SAFETY: the items move into fresh memory; the vector's buffer is freed without dropping them.
        unsafe {
            std::ptr::copy_nonoverlapping(items.as_ptr(), p.as_ptr(), len);
            items.set_len(0);
            std::mem::ManuallyDrop::drop(&mut items);
            std::slice::from_raw_parts_mut(p.as_ptr(), len)
        }
    }

    #[inline]
    #[expect(clippy::mut_from_ref, reason = "a fresh block, as in alloc_with")]
    pub(crate) fn alloc_str(&self, s: &str) -> &mut str {
        let bytes = self.alloc_slice_copy(s.as_bytes());
        // SAFETY: copied from a `str`.
        unsafe { std::str::from_utf8_unchecked_mut(bytes) }
    }

    /// Total size of the chunks (like bumpalo's `allocated_bytes`).
    fn capacity(&self) -> usize {
        self.capacity.get()
    }

    /// A thread arena's chunks for `TSRS_MEM_SPLIT` (`memsplit`): capacity, used bytes, the unused range of the
    /// current chunk and of each retired chunk (start, len), and whether the current chunk is a huge-page chunk.
    pub(crate) fn residency(&self) -> Residency {
        debug_assert!(!self.up);
        let (start, end, ptr) =
            (self.start.get().addr(), self.end.get().addr(), self.ptr.get().addr());
        let retired = self.retired.borrow();
        Residency {
            capacity: self.capacity.get(),
            used: (end - ptr) + retired.iter().map(|&(_, e, f)| e - f).sum::<usize>(),
            current_unused: (ptr > start).then_some((start, ptr - start)),
            retired_unused: retired
                .iter()
                .filter(|&&(s, _, f)| f > s)
                .map(|&(s, _, f)| (s, f - s))
                .collect(),
            current_huge: HUGE_THREAD_CHUNK.is_some_and(|h| end - start >= h),
        }
    }

    /// The used part of every chunk, (start, len).
    pub(crate) fn used_ranges(&self) -> Vec<(usize, usize)> {
        if self.up {
            let mut out =
                vec![(self.start.get().addr(), self.ptr.get().addr() - self.start.get().addr())];
            out.extend(
                self.retired.borrow().iter().map(|&(start, _, finger)| (start, finger - start)),
            );
            return out;
        }
        let mut out = vec![(self.ptr.get().addr(), self.end.get().addr() - self.ptr.get().addr())];
        out.extend(self.retired.borrow().iter().map(|&(_, end, finger)| (finger, end - finger)));
        out
    }

    #[inline]
    pub(crate) fn pop_free(&self, class: usize) -> Option<NonNull<u8>> {
        let head = self.free[class].get();
        if head.is_null() {
            return None;
        }
        // SAFETY: free blocks hold the next pointer in their first word.
        unsafe {
            #[expect(
                clippy::cast_ptr_alignment,
                reason = "free blocks are 8-aligned (push_free's contract)"
            )]
            self.free[class].set(*head.cast_const().cast::<*mut u8>());
            #[cfg(debug_assertions)]
            {
                let size = class * 8;
                let rest = std::slice::from_raw_parts(head.add(8), size - 8);
                assert!(
                    rest.iter().all(|&b| b == POISON),
                    "arena: free block at {head:p} was written after it was freed: {rest:x?}"
                );
            }
            Some(NonNull::new_unchecked(head))
        }
    }

    /// # Safety
    /// `p` points to a dead block of `class * 8` bytes, 8-aligned.
    #[inline]
    unsafe fn push_free(&self, class: usize, p: *mut u8) {
        #[cfg(debug_assertions)]
        // SAFETY: `p` is a dead block of `class * 8` bytes (this function's contract); its first word is kept.
        unsafe {
            std::ptr::write_bytes(p.add(8), POISON, class * 8 - 8);
        };
        #[expect(
            clippy::cast_ptr_alignment,
            reason = "`p` is 8-aligned (this function's contract)"
        )]
        let next = p.cast::<*mut u8>();
        // SAFETY: the block is dead and at least 8 bytes, so its first word holds the free-list link.
        unsafe { *next = self.free[class].get() };
        self.free[class].set(p);
    }

    #[inline]
    pub(crate) fn bump_epoch(&self) {
        self.epoch.set(self.epoch.get() + 1);
    }

    #[inline]
    pub(crate) fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            ptr: self.ptr.get(),
            start: self.start.get(),
            epoch: self.epoch.get(),
            drops: self.drops.borrow().len(),
        }
    }

    /// Whether the range allocated since `cp` can be discarded (`rewindable`).
    #[inline]
    pub(crate) fn rewindable_now(&self, cp: &Checkpoint) -> bool {
        self.rewindable(cp).is_some()
    }

    /// The range allocated since `cp` (lowest address, length) when it can be discarded: same chunk and no free or
    /// pin since.
    #[inline]
    fn rewindable(&self, cp: &Checkpoint) -> Option<(*mut u8, usize)> {
        if cp.start != self.start.get() || cp.epoch != self.epoch.get() {
            return None;
        }
        let now = self.ptr.get();
        if self.up {
            return Some((cp.ptr, now.addr() - cp.ptr.addr()));
        }
        Some((now, cp.ptr.addr() - now.addr()))
    }
}

/// A chunk for a thread's arena or a slab. Compressed pointers: from the process-wide reservation (`reserve`), where
/// `huge` asks for transparent huge pages on Linux.
fn os_chunk(layout: Layout, huge: bool) -> *mut u8 {
    #[cfg(compressed_ptrs)]
    return crate::reserve::alloc_chunk(layout.size(), huge);
    #[cfg(not(compressed_ptrs))]
    let _ = huge;
    #[cfg(not(compressed_ptrs))]
    // SAFETY: non-zero size.
    let base = unsafe { std::alloc::alloc(layout) };
    #[cfg(not(compressed_ptrs))]
    {
        if base.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        base
    }
}

/// Region chunks are carved from per-thread slabs, packed back to back: one region per parsed file means tens of
/// thousands of small regions, and a separate allocation per chunk costs a partly used page at each end (the
/// allocator writes the first word of the next block). A file region's chunk is trimmed to what parse and bind used
/// (`Region::trim`). A slab is released when every chunk carved from it is freed and it is no longer the thread's
/// current slab; large chunks get a slab of their own.
struct Slab {
    base: *mut u8,
    size: usize,
    /// Chunks carved and not yet released, plus one while the slab is a thread's current slab.
    live: AtomicUsize,
    /// A chunk of a retired region (`Region::retire_on_free`) was carved from it: once every chunk is released, its
    /// pages go back to the system but its range is never handed out again (`slab_release`).
    retired: AtomicBool,
}

const SLAB_SIZE: usize = 1 << 20;

/// Slab size of the regions of `Region::new_scratch_in_large_slabs` (the CLI's file regions, most of which are
/// retired once checked). A retired region's pages go back in one system call per run of neighbouring retired pages,
/// and each call flushes the TLB of every core running a thread of the process (`retired`); one thread's file regions
/// in 1 MiB slabs scattered among the other threads' chunks made about 850 calls for vscode's 409 MB of leaves.
/// Untouched pages of a slab cost address space only.
const LARGE_SLAB_SIZE: usize = 16 << 20;

thread_local! {
    /// The thread's current slab and its bump position (upwards).
    static SLAB: CurrentSlab = const { CurrentSlab(Cell::new((std::ptr::null(), 0))) };
    /// The same for large slabs (`LARGE_SLAB_SIZE`).
    static LARGE_SLAB: CurrentSlab = const { CurrentSlab(Cell::new((std::ptr::null(), 0))) };
}

/// A thread's current slab, which holds one of the slab's references until the thread exits. Without the release,
/// every thread that ended kept its current slab: a watch session's checker and worker threads are made per build
/// and end with it, and switching branches on vscode kept about 0.9 GB of the arena address range per round trip.
struct CurrentSlab(Cell<(*const Slab, usize)>);

impl CurrentSlab {
    fn get(&self) -> (*const Slab, usize) {
        self.0.get()
    }

    fn set(&self, v: (*const Slab, usize)) {
        self.0.set(v);
    }
}

impl Drop for CurrentSlab {
    fn drop(&mut self) {
        let (slab, _) = self.0.get();
        if !slab.is_null() {
            slab_release(slab);
        }
    }
}

/// Released standard-size slabs kept for reuse instead of going back to the system (compressed pointers: a
/// decommit and, at the next slab, a commit and fresh page faults). Short-lived regions (one per file during emit)
/// release and take slabs at a high rate. Addresses, at most `SLAB_CACHE_MAX`.
#[cfg(compressed_ptrs)]
static SLAB_CACHE: Mutex<Vec<usize>> = Mutex::new(Vec::new());
#[cfg(compressed_ptrs)]
const SLAB_CACHE_MAX: usize = 64;

fn new_slab(size: usize, live: usize) -> *const Slab {
    let size = size.div_ceil(PAGE) * PAGE;
    #[cfg(compressed_ptrs)]
    if size == SLAB_SIZE {
        let cached = SLAB_CACHE.lock().unwrap().pop();
        if let Some(addr) = cached {
            let base = std::ptr::with_exposed_provenance_mut::<u8>(addr);
            return Box::into_raw(Box::new(Slab {
                base,
                size,
                live: AtomicUsize::new(live),
                retired: AtomicBool::new(false),
            }));
        }
    }
    let base =
        os_chunk(Layout::from_size_align(size, CHUNK_ALIGN).expect("arena slab layout"), false);
    Box::into_raw(Box::new(Slab {
        base,
        size,
        live: AtomicUsize::new(live),
        retired: AtomicBool::new(false),
    }))
}

fn slab_carve(size: usize, large: bool) -> (*mut u8, *const Slab) {
    let (current, slab_size) =
        if large { (&LARGE_SLAB, LARGE_SLAB_SIZE) } else { (&SLAB, SLAB_SIZE) };
    if size > slab_size / 4 {
        return own_slab(size);
    }
    // A thread-local destructor that allocates in a region after the thread's slab was released gets a slab of its own.
    let Ok((mut cur, mut bump)) = current.try_with(CurrentSlab::get) else {
        return own_slab(size);
    };
    // SAFETY: the current slab is kept alive by the thread's reference.
    if cur.is_null() || bump + size > unsafe { (*cur).base.addr() + (*cur).size } {
        if !cur.is_null() {
            slab_release(cur);
        }
        cur = new_slab(slab_size, 1);
        // SAFETY: just made.
        bump = unsafe { (*cur).base.addr() };
    }
    // SAFETY: as above.
    let slab = unsafe { &*cur };
    slab.live.fetch_add(1, Ordering::Relaxed);
    current.with(|s| s.set((cur, bump + size)));
    (slab.base.with_addr(bump), cur)
}

/// A slab for one chunk.
fn own_slab(size: usize) -> (*mut u8, *const Slab) {
    let slab = new_slab(size, 1);
    // SAFETY: just made.
    (unsafe { (*slab).base }, slab)
}

/// Moves the thread's slab position back from `end` to `new_end` if the chunk ending at `end` was its last carve.
fn slab_trim(slab: *const Slab, end: usize, new_end: usize) -> bool {
    [&SLAB, &LARGE_SLAB].into_iter().any(|current| {
        current
            .try_with(|s| {
                let (cur, bump) = s.get();
                if cur == slab && bump == end {
                    s.set((cur, new_end));
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false)
    })
}

fn slab_release(slab: *const Slab) {
    slab_release_ex(slab, false);
}

/// `slab_release`; `retire`: the reference is a chunk of a retired region.
fn slab_release_ex(slab: *const Slab, retire: bool) {
    // SAFETY: the caller holds one of the slab's references.
    let s = unsafe { &*slab };
    if retire {
        // Relaxed: the reference count's AcqRel decrement below orders this store before the last release's load.
        s.retired.store(true, Ordering::Relaxed);
    }
    if s.live.fetch_sub(1, Ordering::AcqRel) != 1 {
        return;
    }
    // Relaxed: every store happened before its thread's decrement, which the AcqRel decrement above acquired.
    if s.retired.load(Ordering::Relaxed) {
        // Its pages go back to the system; its range is never reused (`retired`). Without compressed pointers the
        // slab's memory is kept (it came from the allocator, which would hand the addresses out again).
        #[cfg(all(compressed_ptrs, unix))]
        retired::retire(s.base.addr(), s.size);
        // SAFETY: made by `Box::into_raw` in `new_slab`; this was the last reference.
        drop(unsafe { Box::from_raw(slab.cast_mut()) });
        return;
    }
    #[cfg(not(compressed_ptrs))]
    // SAFETY: allocated by `new_slab` with this layout; every chunk carved from it was released.
    unsafe {
        std::alloc::dealloc(
            s.base,
            Layout::from_size_align(s.size, CHUNK_ALIGN).expect("arena slab layout"),
        );
    };
    #[cfg(compressed_ptrs)]
    {
        let mut cache = SLAB_CACHE.lock().unwrap();
        if s.size == SLAB_SIZE && cache.len() < SLAB_CACHE_MAX {
            cache.push(s.base.expose_provenance());
        } else {
            drop(cache);
            // SAFETY: from `os_chunk` with this size; every chunk carved from it was released.
            unsafe { crate::reserve::release_chunk(s.base, s.size) };
        }
    }
    // SAFETY: made by `Box::into_raw` in `new_slab`; this was the last reference.
    drop(unsafe { Box::from_raw(slab.cast_mut()) });
}

/// 0 unknown, 1 normal, 2 poison (`TSRS_ARENA_POISON=1`): freed and rewound memory is filled and never reused.
static MODE: AtomicU8 = AtomicU8::new(0);

#[inline]
fn poison_mode() -> bool {
    match MODE.load(Ordering::Relaxed) {
        0 => {
            let on = std::env::var_os("TSRS_ARENA_POISON").is_some_and(|v| v == "1");
            MODE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
        m => m == 2,
    }
}

/// The free-list class of a block of `size` bytes and alignment `align` at address `addr`, or 0 when such blocks
/// are not recycled.
#[inline]
pub(crate) const fn free_class(size: usize, align: usize) -> usize {
    if size >= 8 && size <= MAX_FREE_SIZE && size.is_multiple_of(8) && align <= 8 {
        size / 8
    } else {
        0
    }
}

/// Gives a dead block back. Does nothing for sizes that are not recycled.
///
/// # Safety
/// Nothing may use the block afterwards: no live object or local may point into it.
#[inline]
pub(crate) unsafe fn free_block(
    arena: &Arena,
    addr: usize,
    size: usize,
    align: usize,
    needs_drop: bool,
) {
    let class = free_class(size, align);
    if class == 0 || !addr.is_multiple_of(8) {
        return;
    }
    // A region's drop list still names the block; it is dropped (not reused) when the region is freed.
    if needs_drop && arena.is_region() {
        return;
    }
    // Free lists belong to the arena that allocated the block: a recycling site frees only what it made, while the
    // same allocation target (thread arena or region) is current.
    #[cfg(debug_assertions)]
    assert!(
        arena.owns(addr),
        "arena: block {addr:#x} freed into an arena (or region) that did not allocate it"
    );
    // Write access through the chunk's (exposed) provenance, not through the references the program held.
    let p = std::ptr::with_exposed_provenance_mut::<u8>(addr);
    arena.bump_epoch();
    if poison_mode() {
        // SAFETY: the block is `size` dead bytes (this function's contract), written through the chunk's provenance.
        unsafe { std::ptr::write_bytes(p, POISON, size) };
        return;
    }
    // SAFETY: a dead block of `class * 8` bytes (`free_class`), 8-aligned (checked above).
    unsafe { arena.push_free(class, p) };
}

/// Rewinds to `cp` if allowed (see the module docs).
#[inline]
pub(crate) fn rewind(arena: &Arena, cp: Checkpoint) {
    let Some((now, len)) = arena.rewindable(&cp) else {
        return;
    };
    if len == 0 {
        return;
    }
    // Values discarded by the rewind are not dropped (as in a thread arena); forget their drop entries.
    arena.drops.borrow_mut().truncate(cp.drops);
    if poison_mode() {
        // SAFETY: `now .. now + len` is the range allocated since the checkpoint, in the current chunk.
        unsafe { std::ptr::write_bytes(now, POISON, len) };
        return;
    }
    #[cfg(debug_assertions)]
    // SAFETY: as above.
    unsafe {
        std::ptr::write_bytes(now, POISON, len);
    };
    arena.ptr.set(cp.ptr);
}

// ---------------------------------------------------------------------------------------------------------------
// Regions and the allocation target
// ---------------------------------------------------------------------------------------------------------------

/// Registry keys and ends are encoded as addresses + 1 (order is kept).
#[inline]
fn reg_key(addr: usize) -> usize {
    addr + 1
}

/// Chunk start -> (chunk end, region), for `Region::containing` (both as `reg_key`).
static REGISTRY: RwLock<BTreeMap<usize, (usize, Weak<RegionInner>)>> = RwLock::new(BTreeMap::new());
/// Set once the first region exists; until then (the CLI) owner lookups return immediately.
static ANY_REGION: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The arena allocations go to: null until the thread first allocates (then its own arena), or a region.
    pub(crate) static CURRENT: Cell<*const Arena> = const { Cell::new(std::ptr::null()) };
    /// Entered scopes, innermost last: (token, arena). Empty: the thread's own arena is current.
    static SCOPES: RefCell<Vec<(u64, *const Arena)>> = const { RefCell::new(Vec::new()) };
    /// Scope tokens: a scope is `!Send`, so tokens only need to be unique on their thread (a global counter was a
    /// contended cache line once scratch regions push a scope per checker call).
    static NEXT_TOKEN: Cell<u64> = const { Cell::new(1) };
    /// While a scratch region is entered on this thread (`Region::enter_scratch`): its arena, the allocation target
    /// that was current when it was entered (`escape_scratch_one_level`), and the one that was current when the
    /// outermost entered scratch region was entered (`escape_scratch`). Null when none is.
    static SCRATCH: Cell<(*const Arena, *const Arena, *const Arena)> =
        const { Cell::new((std::ptr::null(), std::ptr::null(), std::ptr::null())) };
}

/// A lock that the thread holding it may take again (a region entered while it is already entered).
struct OwnerLock {
    /// (owner, depth, threads waiting).
    state: Mutex<(Option<std::thread::ThreadId>, u32, u32)>,
    released: Condvar,
}

impl OwnerLock {
    fn lock(&self) {
        let me = std::thread::current().id();
        let mut st = self.state.lock().unwrap();
        loop {
            match st.0 {
                None => {
                    st.0 = Some(me);
                    st.1 = 1;
                    return;
                }
                Some(owner) if owner == me => {
                    st.1 += 1;
                    return;
                }
                Some(_) => {
                    st.2 += 1;
                    st = self.released.wait(st).unwrap();
                    st.2 -= 1;
                }
            }
        }
    }

    fn unlock(&self) {
        let mut st = self.state.lock().unwrap();
        st.1 -= 1;
        if st.1 == 0 {
            st.0 = None;
            // A notification is a system call even when nobody waits (std's futex condvar); a file region is entered
            // and left about four times by one thread (parse, bind, two trims) and nearly never waited for.
            let waiting = st.2 != 0;
            drop(st);
            if waiting {
                self.released.notify_one();
            }
        }
    }
}

pub(crate) struct RegionInner {
    arena: Box<Arena>,
    /// Held by the thread that has the region entered: a region's arena is used by one thread at a time.
    lock: OwnerLock,
    /// Addresses of objects outside the region registered as owned by it (`adopt_owner`).
    owners: Mutex<Vec<usize>>,
    /// Run when the region is freed, before anything else (`on_free`).
    on_free: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    /// `retire_on_free`.
    retire: AtomicBool,
    /// `note_escape`.
    escaped: AtomicBool,
}

#[expect(clippy::non_send_fields_in_send_ty, reason = "the arena's cells: see the SAFETY comment")]
// SAFETY: the arena's cells are only touched by the thread holding `lock` (or, before the region is shared, by its
// creator), and by `Drop`, when no handle exists any more.
unsafe impl Send for RegionInner {}
// SAFETY: as for `Send`: shared handles reach the arena's cells only while holding `lock`.
unsafe impl Sync for RegionInner {}

/// A freeable arena (see the module docs). Cloning shares it; it is freed when the last handle is dropped.
#[derive(Clone)]
pub struct Region(Arc<RegionInner>);

impl Region {
    /// A new, empty region whose first chunk has at least `first_chunk` bytes (rounded up to a page).
    pub fn new(first_chunk: usize) -> Region {
        ANY_REGION.store(true, Ordering::Relaxed);
        Region::new_in(first_chunk, true, false)
    }

    /// A region to be used only through `enter_scratch` (one file's emit): like `new`, but its chunks are not in the
    /// registry, so `containing` never returns it and creating or freeing it takes no global lock. Lazily filled data
    /// of an object inside it (`enter_owner`) goes to the current target, the region itself while it is entered. A
    /// process with only scratch regions (the CLI) keeps `enter_owner` / `enter_table_owner` free.
    pub fn new_scratch(first_chunk: usize) -> Region {
        Region::new_in(first_chunk, false, false)
    }

    /// `new_scratch` for one of many regions that are mostly retired (`retire_on_free`) in an order unrelated to
    /// their creation (the CLI's file regions): its chunks are carved from the thread's large slabs
    /// (`LARGE_SLAB_SIZE`), so the regions one thread made lie together and retired neighbours give their pages back
    /// in long runs, one system call (and one TLB flush on every core of the process) each.
    pub fn new_scratch_in_large_slabs(first_chunk: usize) -> Region {
        Region::new_in(first_chunk, false, true)
    }

    fn new_in(first_chunk: usize, registered: bool, large_slabs: bool) -> Region {
        Region(Arc::new_cyclic(|weak| RegionInner {
            arena: Box::new(Arena::with_first_chunk(
                first_chunk.max(PAGE),
                Some(Weak::clone(weak)),
                registered,
                large_slabs,
            )),
            lock: OwnerLock { state: Mutex::new((None, 0, 0)), released: Condvar::new() },
            owners: Mutex::new(Vec::new()),
            on_free: Mutex::new(Vec::new()),
            retire: AtomicBool::new(false),
            escaped: AtomicBool::new(false),
        }))
    }

    /// When the region is freed, its pages go back to the system and its address range is never handed out again
    /// (`reserve::discard`), instead of its slabs being reused: for a region freed while tables keyed by the address
    /// of an object in it may keep stale entries (a checked file's tree, `tsrs_compiler` fileregions.rs). The pages a
    /// chunk shares with a neighbouring chunk of the slab stay until the whole slab is released.
    pub fn retire_on_free(&self) {
        // Relaxed: read by `Drop`, after the last handle's release, which the `Arc` orders after this store.
        self.0.retire.store(true, Ordering::Relaxed);
    }

    /// Makes this region the current thread's allocation target until the scope is dropped. Waits while another
    /// thread has it entered.
    pub fn enter(&self) -> RegionScope {
        self.0.lock.lock();
        RegionScope::push(&raw const *self.0.arena, Some(self.clone()))
    }

    /// The region one of whose chunks contains `addr`.
    pub fn containing(addr: usize) -> Option<Region> {
        if !ANY_REGION.load(Ordering::Relaxed) {
            return None;
        }
        let reg = REGISTRY.read().unwrap();
        let (_, (end, region)) = reg.range(..=reg_key(addr)).next_back()?;
        if reg_key(addr) >= *end {
            return None;
        }
        region.upgrade().map(Region)
    }

    /// `containing` for many addresses under one registry lock.
    pub fn containing_all(addrs: impl Iterator<Item = usize>) -> Vec<Option<Region>> {
        if !ANY_REGION.load(Ordering::Relaxed) {
            return addrs.map(|_| None).collect();
        }
        let reg = REGISTRY.read().unwrap();
        addrs
            .map(|addr| {
                let (_, (end, region)) = reg.range(..=reg_key(addr)).next_back()?;
                if reg_key(addr) >= *end {
                    return None;
                }
                region.upgrade().map(Region)
            })
            .collect()
    }

    /// Runs `f` when the region is freed (also in poison mode, which keeps the memory): for tables
    /// outside the region that must forget pointers into it.
    pub fn on_free(&self, f: Box<dyn FnOnce() + Send>) {
        self.0.on_free.lock().unwrap().push(f);
    }

    /// Registers the object at `addr` (not in any region, e.g. a heap object shared by several region owners) as owned
    /// by this region, so `enter_owner(addr)` routes to it while the region lives.
    pub fn adopt_owner(&self, addr: usize) {
        self.0.owners.lock().unwrap().push(addr);
        REGISTRY
            .write()
            .unwrap()
            .insert(reg_key(addr), (reg_key(addr + 1), Arc::downgrade(&self.0)));
    }

    /// Total size of the region's chunks.
    pub fn allocated_bytes(&self) -> usize {
        self.0.arena.capacity()
    }

    /// Gives the unused end of the region's current chunk back (call on the thread that last allocated in it, right
    /// after a phase that will not allocate much more, such as parsing and binding a file).
    pub fn trim(&self) {
        let _scope = self.enter();
        self.0.arena.trim();
    }

    /// The region's chunks, (start, size) (the shared graph freezes the seed's).
    pub fn chunks(&self) -> Vec<(usize, usize)> {
        self.0.arena.chunks()
    }

    /// Bytes in use in the region's chunks (call while no other thread has the region entered).
    pub fn used_bytes(&self) -> usize {
        self.0.arena.used_ranges().iter().map(|&(_, len)| len).sum()
    }

    /// Forgets the values waiting to be dropped (and frees the list): for a region that will never be freed, whose
    /// values then live for the rest of the process like those of a thread arena. Values allocated afterwards are
    /// tracked again.
    pub fn forget_drops(&self) {
        let _scope = self.enter();
        *self.0.arena.drops.borrow_mut() = Vec::new();
    }

    pub fn ptr_eq(&self, other: &Region) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Whether an object in the region was stored in a structure that may outlive it (`note_escape`).
    pub fn escaped(&self) -> bool {
        self.0.escaped.load(Ordering::Acquire)
    }

    /// `note_escape` for the whole region.
    pub fn mark_escaped(&self) {
        self.0.escaped.store(true, Ordering::Release);
    }
}

impl std::fmt::Debug for Region {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Region({:p})", Arc::as_ptr(&self.0))
    }
}

/// While alive, allocations on this thread go to the scope's arena (a region, or the thread's own arena for
/// `enter_thread_arena`). Scopes nest; dropping one restores the target that was current before it.
pub struct RegionScope {
    token: u64,
    region: Option<Region>,
    // Thread-bound: the scope edits the thread's target stack.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl RegionScope {
    fn push(arena: *const Arena, region: Option<Region>) -> RegionScope {
        let token = NEXT_TOKEN.with(|t| {
            let token = t.get();
            t.set(token + 1);
            token
        });
        SCOPES.with(|s| s.borrow_mut().push((token, arena)));
        CURRENT.with(|c| c.set(arena));
        RegionScope { token, region, _not_send: std::marker::PhantomData }
    }
}

impl Drop for RegionScope {
    fn drop(&mut self) {
        SCOPES.with(|s| {
            let mut s = s.borrow_mut();
            if let Some(i) = s.iter().rposition(|&(t, _)| t == self.token) {
                s.remove(i);
            }
            let top = s.last().map_or(std::ptr::null(), |&(_, a)| a);
            CURRENT.with(|c| c.set(top));
        });
        if let Some(region) = &self.region {
            region.0.lock.unlock();
        }
    }
}

/// Thread arenas given back by threads that are done allocating (`ptr::release_own_arena`), taken by the next thread
/// that needs one (`ptr::own_arena`).
struct SpareArena(&'static Arena);
// SAFETY: a spare arena has no owner: the thread that gave it back reaches it no more (its own-arena slot is cleared
// and no scope names it), and the pool's mutex orders that thread's last use before the next owner's first.
unsafe impl Send for SpareArena {}

static SPARE_ARENAS: Mutex<Vec<SpareArena>> = Mutex::new(Vec::new());

pub(crate) fn take_spare_arena() -> Option<&'static Arena> {
    SPARE_ARENAS.lock().unwrap().pop().map(|s| s.0)
}

/// Whether no region, thread-arena or scratch scope is open on this thread, so that nothing but its own-arena slot
/// names its arena.
pub(crate) fn no_scope_open() -> bool {
    SCOPES.with(|s| s.borrow().is_empty()) && SCRATCH.with(|s| s.get().0.is_null())
}

/// `no_scope_open` while the thread exits: the scope stack may be gone already, and with it every scope.
pub(crate) fn no_scope_open_at_exit() -> bool {
    SCOPES.try_with(|s| s.borrow().is_empty()).unwrap_or(true)
        && SCRATCH.with(|s| s.get().0.is_null())
}

pub(crate) fn give_spare_arena(arena: &'static Arena) {
    CURRENT.with(|c| c.set(std::ptr::null()));
    SPARE_ARENAS.lock().unwrap().push(SpareArena(arena));
}

/// Reclaims unused resident pages at a thread arena's allocation tail.
///
/// Gives back the resident part of the calling thread's own arena that was never handed out: on Linux, when
/// the current chunk is a huge-page chunk, the rest of the 2 MiB block the finger is in (the block was
/// faulted in as one transparent huge page; one `madvise`, which splits that page's mapping).
///
/// For a thread that is done allocating at the moment memory peaks (a checker whose type-check queue ran dry
/// while others still check). A later allocation below the finger faults in fresh zero pages. Elsewhere, and
/// for a 4 KiB-page chunk, the unused part is not resident and this does nothing.
pub fn trim_own_arena_tail() {
    #[cfg(all(compressed_ptrs, target_os = "linux"))]
    {
        let a = crate::ptr::own_arena();
        let (start, end, ptr) = (a.start.get().addr(), a.end.get().addr(), a.ptr.get().addr());
        let Some(huge) = HUGE_THREAD_CHUNK.filter(|&h| end - start >= h) else {
            return;
        };
        let lo = (ptr & !(huge - 1)).max(start);
        if ptr & !(PAGE - 1) > lo {
            // SAFETY: `lo .. ptr` lies in the current chunk below the finger: never handed out, nothing points into it.
            unsafe { crate::reserve::discard(a.ptr.get().with_addr(lo), ptr - lo) };
        }
    }
}

/// The region that is the current thread's allocation target, if any (`None`: the thread's own arena).
pub fn current_region() -> Option<Region> {
    let p = CURRENT.with(std::cell::Cell::get);
    if p.is_null() {
        return None;
    }
    // SAFETY: a non-null `CURRENT` is the thread arena or the arena of a region kept alive by an entered scope.
    let arena = unsafe { &*p };
    arena.region.as_ref().and_then(std::sync::Weak::upgrade).map(Region)
}

/// Keeps a region alive when its objects escape to longer-lived storage.
///
/// Records that the object at `addr` was stored in a structure that may outlive the owner of its region (a
/// program-lifetime cache filled while a checker is held, whose region is the allocation target): the owner
/// must then keep the region until that structure goes (`Region::escaped`; `tsrs_project` checkerpool.rs).
///
/// No-op for an object outside every region, and while no region exists (the CLI).
pub fn note_escape(addr: usize) {
    if let Some(region) = Region::containing(addr) {
        region.mark_escaped();
    }
}

/// Routes lazily filled data by its owner before any registered region exists.
///
/// Makes `enter_owner` and `enter_table_owner` look up the owner's region from now on, as they do once a registered
/// region exists (`Region::new`), for a process that frees unregistered regions (a program's file loader regions,
/// `tsrs_compiler` programregions.rs). Until then they leave lazily filled data of an object outside every region in
/// the current target, which is right only while nothing is freed: a project reference's command line, parsed before
/// any region, got its output maps in the loader region of the first program that read them (an incremental `tsc -b`
/// whose upstream projects were up to date), and kept them after that region was freed.
pub fn route_by_owner() {
    ANY_REGION.store(true, Ordering::Relaxed);
}

/// Makes the current thread's own (never freed) arena the allocation target until the scope is dropped: for data
/// that outlives any region, such as process-wide lazily initialized statics.
pub fn enter_thread_arena() -> RegionScope {
    let own = std::ptr::from_ref::<Arena>(crate::ptr::own_arena());
    RegionScope::push(own, None)
}

/// Keeps lazily allocated data in its owning object's region.
///
/// Routes allocations to the region that owns the object at `addr` (lazily initialized data of a shared
/// object, such as a source file's `JSDoc` cache, must live and die with that object, not with the checker
/// that happens to fill it).
///
/// An object outside every region gets the thread's own arena. No scope (and no cost beyond one load) while
/// no region exists, as in the CLI.
#[inline]
pub fn enter_owner(addr: usize) -> Option<RegionScope> {
    if !ANY_REGION.load(Ordering::Relaxed) {
        // Scratch regions are not registered: data of an object outside the scratch region must not land in it.
        return escape_scratch_unless_inside(addr);
    }
    Some(match Region::containing(addr) {
        Some(region) => region.enter(),
        // Scratch regions are not registered: an object of the one entered on this thread (a checker's, a file's
        // emit) keeps its data there, as when no region is registered, instead of leaking it into the thread arena.
        None if scratch_contains(addr) => return None,
        None => enter_thread_arena(),
    })
}

/// Keeps shared-table allocations alive across program versions.
///
/// Routes allocations for a value stored in a shared table that may outlive the current allocation target (a
/// cache that later program versions copy, filled by whichever thread happens to look something up): they
/// stay in the current target only if the table at `addr` lives in it, and go to the thread's own (never
/// freed) arena otherwise.
///
/// Unlike `enter_owner`, never waits for a region another thread has entered. No scope (and no cost beyond
/// one load) while no region exists, as in the CLI.
#[inline]
pub fn enter_table_owner(addr: usize) -> Option<RegionScope> {
    if !ANY_REGION.load(Ordering::Relaxed) {
        return escape_scratch_unless_inside(addr);
    }
    let current = CURRENT.with(std::cell::Cell::get);
    match Region::containing(addr) {
        Some(region) if std::ptr::eq(&raw const *region.0.arena, current) => None,
        // As in `enter_owner`: a table in the scratch region entered on this thread, which is the current target.
        None if std::ptr::eq(scratch_arena(), current) && scratch_contains(addr) => None,
        _ => Some(enter_thread_arena()),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Scratch regions (per-file emit, notes/mem-emit-regions.md)
// ---------------------------------------------------------------------------------------------------------------

/// Reclaims temporary allocations together when a scratch scope ends.
///
/// While alive, the region it was made from is the thread's allocation target as with `Region::enter`, and
/// also the thread's **scratch region**: code that knows its allocations die with the scratch region
/// allocates there even while escaped (`P::new_scratch`, `alloc_str_scratch`, ...), and code whose
/// allocations must outlive it escapes to the target that was current before it (`escape_scratch`).
///
/// Emit uses one per file: the transformers, the printer and the node builder's per-call state allocate in
/// it, the checker escapes (its types, symbols and caches outlive the file).
pub struct ScratchScope {
    saved: (*const Arena, *const Arena, *const Arena),
    _scope: RegionScope,
}

impl Region {
    /// `enter`, and makes the region the thread's scratch region (see `ScratchScope`). Scratch scopes nest; the
    /// innermost one is the scratch region.
    pub fn enter_scratch(&self) -> ScratchScope {
        let outer = CURRENT.with(std::cell::Cell::get);
        let outer = if outer.is_null() {
            std::ptr::from_ref::<Arena>(crate::ptr::own_arena())
        } else {
            outer
        };
        let scope = self.enter();
        let saved = SCRATCH.with(|s| {
            let (enclosing, _, enclosing_root) = s.get();
            let root = if enclosing.is_null() { outer } else { enclosing_root };
            s.replace((&raw const *self.0.arena, outer, root))
        });
        ScratchScope { saved, _scope: scope }
    }
}

impl Drop for ScratchScope {
    fn drop(&mut self) {
        SCRATCH.with(|s| s.set(self.saved));
    }
}

/// Moves escaping allocations outside all enclosing scratch regions.
///
/// Inside scratch regions, makes the allocation target that was current when the outermost of them was
/// entered (the thread's own arena, or an enclosing region that is not a scratch region) the target until the
/// scope is dropped: for code whose allocations may outlive every scratch region (program caches,
/// diagnostics, the lazily filled data of shared objects).
///
/// Scratch regions nest when a checker that runs in a region of its own (checkerpool.rs `SlotChecker`) emits
/// a file (`program_emit.rs`): what escapes the file's region must outlive the checker too, as an incremental
/// snapshot keeps the diagnostics after their program's checkers are freed (watch mode). `None` (no cost
/// beyond a thread-local read) outside scratch regions or when that target is already current.
#[inline]
pub fn escape_scratch() -> Option<RegionScope> {
    let (scratch, _, root) = SCRATCH.with(std::cell::Cell::get);
    if scratch.is_null() || CURRENT.with(std::cell::Cell::get) == root {
        return None;
    }
    Some(RegionScope::push(root, None))
}

/// Keeps allocations alive beyond the innermost scratch region.
///
/// `escape_scratch` to the target that was current when the innermost scratch region was entered: for the
/// checker called from emit, whose types, symbols and caches outlive one file's emit region and belong to the
/// checker's own region when it has one.
#[inline]
pub fn escape_scratch_one_level() -> Option<RegionScope> {
    let (scratch, outer, _) = SCRATCH.with(std::cell::Cell::get);
    if scratch.is_null() || CURRENT.with(std::cell::Cell::get) == outer {
        return None;
    }
    Some(RegionScope::push(outer, None))
}

thread_local! {
    /// `Region::route_diagnostics`: (the allocation target the route applies to, the arena the diagnostics go to);
    /// null while no route is set.
    static DIAGNOSTIC_ROUTE: Cell<(*const Arena, *const Arena)> = const { Cell::new((std::ptr::null(), std::ptr::null())) };
}

/// A route set by `Region::route_diagnostics`, until dropped.
pub struct DiagnosticRoute {
    saved: (*const Arena, *const Arena),
    _region: Region,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Region {
    /// Until the route is dropped, diagnostics made on this thread while the allocation target is the current one
    /// (`escape_for_diagnostic`) go to this region instead of escaping the thread's scratch regions: for a session
    /// that frees what it no longer uses (watch mode), whose checkers would otherwise leave every diagnostic they
    /// report in the thread arenas (a few thousand per cycle on vscode). Diagnostics made under another target (the
    /// lazily filled data of a file, `enter_owner`; a program cache, `escape_scratch`) are not routed.
    ///
    /// The route allocates in the region without entering it: while the route is set, no other thread may allocate
    /// in it.
    pub fn route_diagnostics(&self) -> DiagnosticRoute {
        let saved =
            DIAGNOSTIC_ROUTE.with(|r| r.replace((current_target(), &raw const *self.0.arena)));
        DiagnosticRoute { saved, _region: self.clone(), _not_send: std::marker::PhantomData }
    }
}

impl Drop for DiagnosticRoute {
    fn drop(&mut self) {
        DIAGNOSTIC_ROUTE.with(|r| r.set(self.saved));
    }
}

/// For the allocations of a diagnostic: its route's region (`Region::route_diagnostics`) when the allocation target
/// is the one the route applies to, else `escape_scratch`.
#[inline]
pub fn escape_for_diagnostic() -> Option<RegionScope> {
    let (from, to) = DIAGNOSTIC_ROUTE.with(std::cell::Cell::get);
    if !from.is_null() && current_target() == from {
        return Some(RegionScope::push(to, None));
    }
    escape_scratch()
}

/// The allocation target: the current region's arena, or the thread's own.
fn current_target() -> *const Arena {
    let cur = CURRENT.with(std::cell::Cell::get);
    if cur.is_null() { std::ptr::from_ref::<Arena>(crate::ptr::own_arena()) } else { cur }
}

/// `escape_scratch` unless the object at `addr` lives in the scratch region (`enter_owner` without registered regions).
#[inline]
fn escape_scratch_unless_inside(addr: usize) -> Option<RegionScope> {
    if scratch_arena().is_null() || scratch_contains(addr) {
        return None;
    }
    escape_scratch()
}

/// Whether a scratch region is entered on this thread.
#[inline]
pub fn scratch_active() -> bool {
    !SCRATCH.with(std::cell::Cell::get).0.is_null()
}

/// Whether `addr` lies in the thread's scratch region (for caches outside it that must forget keys in it).
pub fn scratch_contains(addr: usize) -> bool {
    let s = scratch_arena();
    if s.is_null() {
        return false;
    }
    // SAFETY: the scratch region is kept alive by its entered scope on this thread, which alone uses its arena.
    let a = unsafe { &*s };
    (a.start.get().addr() <= addr && addr < a.end.get().addr())
        || a.retired.borrow().iter().any(|&(start, end, _)| start <= addr && addr < end)
}

/// The thread's scratch region's arena, if one is entered.
#[inline]
pub(crate) fn scratch_arena() -> *const Arena {
    SCRATCH.with(std::cell::Cell::get).0
}

impl Drop for RegionInner {
    fn drop(&mut self) {
        for f in self.on_free.get_mut().unwrap().drain(..) {
            f();
        }
        let arena = &*self.arena;
        let chunks = arena.chunks();
        if arena.registered || !self.owners.get_mut().unwrap().is_empty() {
            let mut reg = REGISTRY.write().unwrap();
            for &(start, _) in &chunks {
                reg.remove(&reg_key(start));
            }
            for addr in self.owners.get_mut().unwrap().drain(..) {
                reg.remove(&reg_key(addr));
            }
        }
        let drops = std::mem::take(&mut *arena.drops.borrow_mut());
        for d in drops {
            // SAFETY: `len` values of the entry's type were allocated at `ptr` in this region and never dropped (a
            // freed block of a type that needs drop is not reused, a rewound one has no entry).
            unsafe { (d.drop)(d.ptr, d.len) };
        }
        if poison_mode() {
            // Kept mapped and filled, so any later use through a stale pointer crashes.
            for (start, len) in arena.used_ranges() {
                // SAFETY: the used part of a chunk of this region; nothing may use it any more.
                unsafe {
                    std::ptr::write_bytes(
                        std::ptr::with_exposed_provenance_mut::<u8>(start),
                        POISON,
                        len,
                    );
                };
            }
            return;
        }
        // Relaxed: see `retire_on_free`.
        let retire = self.retire.load(Ordering::Relaxed);
        #[cfg(all(compressed_ptrs, unix))]
        if retire {
            for &(start, size) in &chunks {
                retired::retire(start, size);
            }
        }
        drop(chunks);
        for &slab in arena.slabs.borrow().iter() {
            slab_release_ex(slab, retire);
        }
    }
}

/// Gives back the pages of retired regions (`Region::retire_on_free`) that nothing uses any more, without
/// ever giving their ranges back (`reserve::discard`).
///
/// Retired ranges (the chunks of retired regions, and retired slabs once their last chunk goes) are kept as
/// coalesced spans, so the page that two neighbouring chunks share goes once both are retired: file regions
/// are carved back to back, leaves are often neighbours (vscode: 4,691 leaf chunks form 924 runs), and with
/// 16 KiB pages the shared pages are a tenth of what the leaves hold. They are given back in batches of
/// `BATCH` bytes (`flush_retired` gives back the rest): each `madvise` / `mmap` call flushes the TLB of every
/// core running a thread of the process, and one call per chunk cost the check pass 2-3% at 32 checkers on
/// Linux, against one per run of neighbouring pages in a batch.
#[cfg(all(compressed_ptrs, unix))]
pub mod retired {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// Bytes retired before a batch is given back.
    const BATCH: usize = 16 << 20;

    pub struct State {
        /// Retired spans, start -> end, coalesced (touching or overlapping ranges merge).
        pub spans: BTreeMap<usize, usize>,
        /// Retired since the last batch, not given back yet.
        pub pending: Vec<(usize, usize)>,
        pub pending_bytes: usize,
    }

    static STATE: Mutex<State> =
        Mutex::new(State { spans: BTreeMap::new(), pending: Vec::new(), pending_bytes: 0 });

    /// Retires `start .. start + len` (nothing uses it any more, and it is never handed out again).
    pub(super) fn retire(start: usize, len: usize) {
        let calls = {
            let mut s = STATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.pending.push((start, start + len));
            s.pending_bytes += len;
            if s.pending_bytes < BATCH {
                return;
            }
            let pages = take_pages(&mut s);
            spans_of(&s, &pages)
        };
        give_back(&calls);
    }

    pub(super) fn flush() {
        let calls = {
            let mut s = STATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let pages = take_pages(&mut s);
            spans_of(&s, &pages)
        };
        give_back(&calls);
    }

    /// The whole pages of each span that `pages` (from `take_pages`) lie in: a span's pages are all retired, and the
    /// ones it absorbed were given back before (they hold no memory, and giving them back again costs nothing), so
    /// one call per span gives back what `pages` lists. Freed with many checkers, a batch's ranges sit between ranges
    /// retired earlier, and their pages would take a call for each piece; each call flushes the TLB of every core
    /// running a thread of the process.
    #[expect(
        clippy::too_long_first_doc_paragraph,
        reason = "The integration tests reach this helper; its comment stays one paragraph."
    )]
    pub fn spans_of(s: &State, pages: &[(usize, usize)]) -> Vec<(usize, usize)> {
        let page = crate::reserve::page_size();
        let mut out: Vec<(usize, usize)> = Vec::with_capacity(pages.len());
        for &(first, _) in pages {
            let Some((&lo, &hi)) = s.spans.range(..=first).next_back() else {
                continue;
            };
            let whole = (lo.next_multiple_of(page), hi & !(page - 1));
            if out.last() != Some(&whole) {
                out.push(whole);
            }
        }
        out
    }

    /// Merges the pending ranges into the spans and returns the pages that became wholly retired: for each pending
    /// range, the whole pages of its coalesced span minus those of the spans it absorbed (given back already, or
    /// earlier in this batch), merged where they touch.
    #[expect(
        clippy::too_long_first_doc_paragraph,
        reason = "The integration tests reach this helper; its comment stays one paragraph."
    )]
    pub fn take_pages(s: &mut State) -> Vec<(usize, usize)> {
        let page = crate::reserve::page_size();
        let interior = |lo: usize, hi: usize| (lo.next_multiple_of(page), hi & !(page - 1));
        let mut pages = Vec::new();
        for (start, end) in std::mem::take(&mut s.pending) {
            let (mut lo, mut hi) = (start, end);
            let mut absorbed: Vec<(usize, usize)> = Vec::new();
            if let Some((&ps, &pe)) = s.spans.range(..=start).next_back()
                && pe >= start
            {
                absorbed.push((ps, pe));
                lo = ps;
                hi = hi.max(pe);
                s.spans.remove(&ps);
            }
            while let Some((&ns, &ne)) = s.spans.range(lo..).next() {
                if ns > hi {
                    break;
                }
                absorbed.push((ns, ne));
                hi = hi.max(ne);
                s.spans.remove(&ns);
            }
            s.spans.insert(lo, hi);
            // The span's whole pages, minus the absorbed spans' (sorted, disjoint, inside it).
            let (mut next, last) = interior(lo, hi);
            for (a, b) in absorbed {
                let (done_first, done_last) = interior(a, b);
                if done_last <= done_first {
                    continue;
                }
                if done_first > next {
                    pages.push((next, done_first.min(last)));
                }
                next = next.max(done_last);
            }
            if last > next {
                pages.push((next, last));
            }
        }
        pages.retain(|&(first, last)| last > first);
        s.pending_bytes = 0;
        pages.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(pages.len());
        for (first, last) in pages {
            match merged.last_mut() {
                Some(prev) if prev.1 >= first => prev.1 = prev.1.max(last),
                _ => merged.push((first, last)),
            }
        }
        merged
    }

    /// Gives back `calls` (whole pages of retired spans, `spans_of`).
    fn give_back(calls: &[(usize, usize)]) {
        for &(first, last) in calls {
            // SAFETY: whole pages inside retired spans: chunks and slabs nothing uses any more, never handed out
            // again; `first` is page aligned (and so is `last - first`).
            unsafe {
                crate::reserve::discard(
                    std::ptr::with_exposed_provenance_mut::<u8>(first),
                    last - first,
                );
            };
        }
    }
}

/// Gives back the pages of the regions retired since the last batch (`Region::retire_on_free`), for a caller that has
/// freed the last of them (the end of the type-check pass).
pub fn flush_retired() {
    #[cfg(all(compressed_ptrs, unix))]
    retired::flush();
}
