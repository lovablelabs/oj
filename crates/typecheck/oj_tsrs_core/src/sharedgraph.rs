//! tsrs-only: a frozen seed graph shared by every checker thread (`--features tsrs_core/shared-graph`, on with
//! `TSRS_SHARED_GRAPH=1`; `tsrs_compiler` `sharedgraph` drives it, notes/spike-shared-graph.md).
//!
//! One seed checker runs in a region of its own; when it is done the region's chunks are *frozen*: marked in a page
//! bitmap (`is_frozen_addr`) and `mprotect`ed read-only (`TSRS_SHARED_GRAPH_PROTECT=0` turns that off), so a missed
//! write is a fault with a backtrace (except on musl) instead of a silent race. Every pool checker
//! is a fork that reads the frozen objects and keeps what it would have written into them in its own `Overlay`, keyed
//! by the address of the cell. `OvCell` is a `Cell` whose writes to a frozen address go to the overlay of the
//! checker the current thread runs (`enter_overlay`).
//!
//! Nothing here is used unless the switch is on: `ANY_FROZEN` stays false and every `OvCell` behaves as a `Cell`.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use rustc_hash::FxHashMap;

/// The system page size (`mprotect` granularity; every caller is unix-only).
#[cfg(unix)]
fn page_size() -> usize {
    // SAFETY: sysconf has no preconditions.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

/// Whether the shared graph is compiled in (`--features tsrs_core/shared-graph`). Without it every frozen-object test below is a
/// constant false, so the switch-off build runs main's code paths.
pub const COMPILED_IN: bool = cfg!(feature = "shared-graph");

/// Set once, by `freeze`, before any fork exists (the forks are created on threads spawned after it).
static ANY_FROZEN: AtomicBool = AtomicBool::new(false);

#[cfg(compressed_ptrs)]
const PAGE_SHIFT: usize = 12;
#[cfg(compressed_ptrs)]
const SPAN: usize = 32 << 30;
#[cfg(compressed_ptrs)]
const WORDS: usize = SPAN >> PAGE_SHIFT >> 6;
/// One bit per 4 KiB page of the reservation (1 MiB, zero until a freeze, in .bss).
#[cfg(compressed_ptrs)]
static FROZEN: [AtomicU64; WORDS] = [const { AtomicU64::new(0) }; WORDS];

/// Plain pointers (no fixed reservation to keep a page bitmap for): the frozen chunks as sorted `(start, end)` ranges,
/// one leaked slice behind one pointer, so a reader never sees a slice and a length from two freezes. Ranges are added
/// as the bitmap's bits are (a later freeze keeps the earlier ones). Without them every object counted as unfrozen
/// under `plain-ptrs`, and forks wrote their lazily filled fields into the shared seed: a write fault on its protected
/// pages, and with `TSRS_SHARED_GRAPH_PROTECT=0` data races between forks (`ThreadSanitizer`, which needs plain pointers).
#[cfg(not(compressed_ptrs))]
static FROZEN_RANGES: std::sync::atomic::AtomicPtr<Box<[(usize, usize)]>> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// The frozen chunks lie in `[FROZEN_LO, FROZEN_LO + FROZEN_SPAN)` (0 and 0 while nothing is frozen).
static FROZEN_LO: AtomicUsize = AtomicUsize::new(0);
static FROZEN_SPAN: AtomicUsize = AtomicUsize::new(0);
/// One bit per 64-byte line of the frozen range: some fork wrote a cell in that line into its overlay. A read of a
/// frozen cell whose line is clean returns the inline value without touching the overlay (no thread-local, no hash).
/// Bits are only ever set (a fork whose overlay lacks the cell falls back to the inline value).
static DIRTY: std::sync::atomic::AtomicPtr<AtomicU64> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Whether the cell at `addr` is frozen and its line is dirty: the only case a read must consult the overlay.
#[inline(always)]
#[expect(
    clippy::inline_always,
    reason = "on every read of a lazy field; without the feature it must fold to `false`"
)]
pub fn dirty(addr: usize) -> bool {
    if !COMPILED_IN {
        return false;
    }
    // Acquire: pairs with `freeze`'s Release store of the span, so a thread that sees the span also sees the start and
    // the dirty bitmap (checkers may run while the seed is frozen, not only forks spawned after it).
    let span = FROZEN_SPAN.load(Ordering::Acquire);
    // Relaxed: ordered by the Acquire load above.
    let off = addr.wrapping_sub(FROZEN_LO.load(Ordering::Relaxed));
    if off >= span {
        return false;
    }
    let line = off >> 6;
    // SAFETY: DIRTY covers the whole span (allocated by `freeze` before FROZEN_SPAN became non-zero).
    let w = unsafe { &*DIRTY.load(Ordering::Relaxed).add(line >> 6) };
    // Relaxed: a stale clear bit only means this thread never wrote the line (its own writes are program-ordered).
    w.load(Ordering::Relaxed) & (1 << (line & 63)) != 0
}

#[inline]
fn mark_dirty(addr: usize) {
    // Relaxed: the range was written before the forks' threads were spawned.
    let line = (addr - FROZEN_LO.load(Ordering::Relaxed)) >> 6;
    // SAFETY: as in `dirty`; the caller checked that `addr` is frozen.
    let w = unsafe { &*DIRTY.load(Ordering::Relaxed).add(line >> 6) };
    let bit = 1 << (line & 63);
    // Relaxed (both): a bit only tells readers to look in their own overlay, which holds only their own writes; a
    // reader that misses another fork's bit has nothing of that fork's to find.
    if w.load(Ordering::Relaxed) & bit == 0 {
        w.fetch_or(bit, Ordering::Relaxed);
    }
}

#[inline(always)]
#[expect(
    clippy::inline_always,
    reason = "on every write of a lazy field; without the feature it must fold to `false`"
)]
pub fn any_frozen() -> bool {
    // Relaxed: written before the threads that read it were spawned.
    COMPILED_IN && ANY_FROZEN.load(Ordering::Relaxed)
}

#[inline]
fn is_frozen_addr_slow(addr: usize) -> bool {
    if !COMPILED_IN {
        return false;
    }
    // Acquire: as in `dirty` (it also orders the page bitmap `FROZEN`, written before the span).
    let span = FROZEN_SPAN.load(Ordering::Acquire);
    // Relaxed: ordered by the Acquire load above.
    if addr.wrapping_sub(FROZEN_LO.load(Ordering::Relaxed)) >= span {
        return false;
    }
    #[cfg(compressed_ptrs)]
    {
        let off = addr.wrapping_sub(crate::reserve::BASE_ADDR);
        if off >= SPAN {
            return false;
        }
        let page = off >> PAGE_SHIFT;
        // Relaxed: written before the reading threads were spawned.
        FROZEN[page >> 6].load(Ordering::Relaxed) & (1 << (page & 63)) != 0
    }
    #[cfg(not(compressed_ptrs))]
    {
        // Acquire: pairs with `freeze`'s Release store of the ranges.
        let ranges = FROZEN_RANGES.load(Ordering::Acquire);
        if ranges.is_null() {
            return false;
        }
        // SAFETY: a leaked, never freed slice (`freeze`).
        let ranges = unsafe { &**ranges };
        let i = ranges.partition_point(|&(start, _)| start <= addr);
        i > 0 && addr < ranges[i - 1].1
    }
}

/// Whether frozen chunks are `mprotect`ed (`TSRS_SHARED_GRAPH_PROTECT`, default on; `0` turns it off).
#[cfg(unix)]
fn protect() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(std::env::var("TSRS_SHARED_GRAPH_PROTECT").as_deref(), Ok("0" | "off"))
    })
}

/// Marks `ranges` (start, len; page-aligned chunks of the seed's region) frozen, and protects them unless
/// `TSRS_SHARED_GRAPH_PROTECT=0`.
pub fn freeze(ranges: &[(usize, usize)]) {
    #[cfg(compressed_ptrs)]
    for &(start, len) in ranges {
        let off = start - crate::reserve::BASE_ADDR;
        assert!(off + len <= SPAN, "shared graph: frozen chunk outside the reservation");
        // Every 4 KiB page the chunk touches, partial ones included: a seed object in a chunk's last, partial page is as
        // frozen as any other (its lazily filled fields must go to the forks' overlays, not inline, where one fork's
        // value would be every fork's and outlive the fork). The seed's chunks are large and each has a slab of its
        // own, rounded to whole 4 KiB pages, so the rest of such a page holds no one else's objects.
        for page in (off >> PAGE_SHIFT)..(off + len).div_ceil(1 << PAGE_SHIFT) {
            // Relaxed: published to the forks by spawning their threads.
            FROZEN[page >> 6].fetch_or(1 << (page & 63), Ordering::Relaxed);
        }
    }
    #[cfg(not(compressed_ptrs))]
    {
        // Acquire: pairs with the Release store below of an earlier freeze.
        let old = FROZEN_RANGES.load(Ordering::Acquire);
        let mut all: Vec<(usize, usize)> = if old.is_null() {
            Vec::new()
        } else {
            // SAFETY: a leaked, never freed slice (stored below).
            unsafe { (*old).to_vec() }
        };
        all.extend(ranges.iter().map(|&(start, len)| (start, start + len)));
        all.sort_unstable();
        // Release: a reader that loads the pointer (Acquire) sees the slice it points to.
        FROZEN_RANGES.store(Box::into_raw(Box::new(all.into_boxed_slice())), Ordering::Release);
    }
    let lo = ranges.iter().map(|r| r.0).min().unwrap_or(0);
    let hi = ranges.iter().map(|r| r.0 + r.1).max().unwrap_or(0);
    let words = ((hi - lo) >> 6).div_ceil(64) + 1;
    let dirty: &'static mut [AtomicU64] =
        Box::leak(std::iter::repeat_with(|| AtomicU64::new(0)).take(words).collect());
    // Relaxed (both): published by the Release store of the span below.
    DIRTY.store(dirty.as_mut_ptr(), Ordering::Relaxed);
    FROZEN_LO.store(lo, Ordering::Relaxed);
    // Release: a reader that sees the span (Acquire) sees the start, the dirty bitmap and the page bitmap.
    FROZEN_SPAN.store(hi - lo, Ordering::Release);
    // Release: orders the range and bitmap stores above before it (readers are spawned later anyway).
    ANY_FROZEN.store(true, Ordering::Release);
    #[cfg(unix)]
    if protect() {
        install_fault_handler();
        let page = page_size();
        for &(start, len) in ranges {
            // Only whole system pages inside the chunk: a 16 KiB page may also hold the neighbouring chunk.
            let (lo, hi) = (start.next_multiple_of(page), (start + len) & !(page - 1));
            if hi <= lo {
                continue;
            }
            // SAFETY: whole pages of a chunk of the reservation, mapped read-write; they become read-only.
            let r = unsafe {
                libc::mprotect(
                    std::ptr::with_exposed_provenance_mut::<libc::c_void>(lo),
                    hi - lo,
                    libc::PROT_READ,
                )
            };
            assert!(r == 0, "shared graph: mprotect failed");
        }
    }
}

#[cfg(unix)]
fn install_fault_handler() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for sig in [libc::SIGSEGV, libc::SIGBUS] {
            // SAFETY: installs a handler with a valid sigaction; the handler only reads `si_addr`.
            unsafe {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = fault_handler as *const () as usize;
                sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
                libc::sigemptyset(&raw mut sa.sa_mask);
                libc::sigaction(sig, &raw const sa, std::ptr::null_mut());
            }
        }
    });
}

#[cfg(unix)]
extern "C" fn fault_handler(sig: i32, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    // musl has no execinfo backtrace functions; it still gets the fault message and the same signal handling.
    #[cfg(not(target_env = "musl"))]
    unsafe extern "C" {
        fn backtrace(buf: *mut *mut libc::c_void, size: i32) -> i32;
        fn backtrace_symbols_fd(buf: *const *mut libc::c_void, size: i32, fd: i32);
    }
    // SAFETY: the kernel passes a valid siginfo for SA_SIGINFO handlers.
    let addr = unsafe { (*info).si_addr() }.addr();
    let ours = is_frozen_addr_slow(addr);
    let msg: &[u8] = if ours {
        b"tsrs shared graph: write to frozen address\n"
    } else {
        b"tsrs shared graph: fault outside the frozen seed\n"
    };
    // SAFETY: writes a static buffer to stderr; fills and prints a local frame buffer (the raw frames, symbolized by
    // the system without allocating).
    unsafe {
        libc::write(2, msg.as_ptr().cast(), msg.len());
        #[cfg(not(target_env = "musl"))]
        {
            let mut frames = [std::ptr::null_mut::<libc::c_void>(); 64];
            let n = backtrace(frames.as_mut_ptr(), 64);
            backtrace_symbols_fd(frames.as_ptr(), n, 2);
        }
    }
    if ours {
        std::process::abort();
    }
    // Not ours: the default action (a crash) when the faulting access is retried on return.
    // SAFETY: resets this signal's disposition.
    unsafe { libc::signal(sig, libc::SIG_DFL) };
}

/// A checker's private values for frozen cells and its private tables hung off frozen objects.
#[derive(Default)]
pub struct Overlay {
    cells: RefCell<FxHashMap<usize, [u64; 2]>>,
    /// One bit per 64 bytes of the frozen range: some cell in that line has an overlay value (skips the hash lookup).
    lines: RefCell<Vec<u64>>,
    id_words: RefCell<Vec<Option<Box<[u64; ID_PAGE]>>>>,
}

impl Overlay {
    #[inline]
    fn line(addr: usize) -> usize {
        // Relaxed: set before the forks' threads were spawned.
        (addr - FROZEN_LO.load(Ordering::Relaxed)) >> 6
    }

    #[inline]
    fn get_cell<T: Copy>(&self, addr: usize, v: T) -> T {
        let line = Self::line(addr);
        let has = self.lines.borrow().get(line >> 6).is_some_and(|w| w & (1 << (line & 63)) != 0);
        if !has {
            return v;
        }
        self.cells.borrow().get(&addr).map_or(v, |&b| decode(b))
    }

    fn set_cell<T: Copy>(&self, addr: usize, v: T) {
        let line = Self::line(addr);
        {
            let mut lines = self.lines.borrow_mut();
            if lines.is_empty() {
                // Relaxed: as in `line`.
                let span = FROZEN_SPAN.load(Ordering::Relaxed);
                lines.resize((span >> 6).div_ceil(64) + 1, 0);
            }
            lines[line >> 6] |= 1 << (line & 63);
        }
        mark_dirty(addr);
        self.cells.borrow_mut().insert(addr, encode(v));
    }
}

thread_local! {
    static CURRENT: Cell<*const Overlay> = const { Cell::new(std::ptr::null()) };
}

/// Makes `overlay` the one `OvCell`s on this thread use. The pool calls it wherever it locks a checker.
pub fn enter_overlay(overlay: &Overlay) {
    CURRENT.with(|c| c.set(std::ptr::from_ref(overlay)));
}

/// Forgets the overlay `enter_overlay` set, for a thread that outlives its checker (a reused checker thread).
pub fn leave_overlay() {
    CURRENT.with(|c| c.set(std::ptr::null()));
}

/// The overlay of the checker this thread runs, if any. Without one (the main thread sorting diagnostics after the
/// pass), frozen objects read as the seed left them.
#[inline]
pub fn try_current_overlay() -> Option<&'static Overlay> {
    let p = CURRENT.with(Cell::get);
    // SAFETY: set by `enter_overlay` from a checker that outlives its use on this thread.
    (!p.is_null()).then(|| unsafe { &*p })
}

fn current_overlay() -> &'static Overlay {
    let p = CURRENT.with(Cell::get);
    assert!(
        !p.is_null(),
        "shared graph: a frozen object was touched on a thread without a checker overlay"
    );
    // SAFETY: set by `enter_overlay` from a checker that outlives its use on this thread.
    unsafe { &*p }
}

#[inline]
fn encode<T: Copy>(v: T) -> [u64; 2] {
    const { assert!(std::mem::size_of::<T>() <= 16) };
    let mut out = [0u64; 2];
    // SAFETY: T fits in 16 bytes; a plain byte copy of a Copy value.
    unsafe {
        std::ptr::copy_nonoverlapping(
            std::ptr::from_ref(&v).cast::<u8>(),
            out.as_mut_ptr().cast::<u8>(),
            std::mem::size_of::<T>(),
        );
    };
    out
}

#[inline]
fn decode<T: Copy>(bits: [u64; 2]) -> T {
    // SAFETY: `bits` was made by `encode::<T>` of a valid T.
    unsafe { std::ptr::read_unaligned(bits.as_ptr().cast::<T>()) }
}

/// A `Cell` for a lazily filled field of an arena object that may be frozen (see the module docs).
#[repr(transparent)]
pub struct OvCell<T>(Cell<T>);

impl<T: Copy + Default + PartialEq> Default for OvCell<T> {
    fn default() -> Self {
        OvCell(Cell::new(T::default()))
    }
}

impl<T: Copy + std::fmt::Debug> std::fmt::Debug for OvCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.get().fmt(f)
    }
}

impl<T: Copy + Default + PartialEq> OvCell<T> {
    #[inline]
    const fn new(v: T) -> Self {
        OvCell(Cell::new(v))
    }

    /// A set value is returned as is (the seed's value wins over a fork's later write of another one); an unset one is looked up in the overlay only if its line is dirty.
    #[inline]
    pub fn get(&self) -> T {
        let v = self.0.get();
        if v != T::default() || !dirty(std::ptr::from_ref(self).addr()) {
            return v;
        }
        overlay_get_frozen(std::ptr::from_ref(self).addr(), v)
    }

    #[inline]
    pub fn set(&self, v: T) {
        if any_frozen() && self.set_frozen(v) {
            return;
        }
        self.0.set(v);
    }

    /// Writes to the overlay if the cell is frozen (returns true).
    #[cold]
    #[inline(never)]
    fn set_frozen(&self, v: T) -> bool {
        let addr = std::ptr::from_ref(self).addr();
        if !is_frozen_addr_slow(addr) {
            return false;
        }
        current_overlay().set_cell(addr, v);
        true
    }
}

impl<T: Copy + Default + PartialEq> Clone for OvCell<T> {
    fn clone(&self) -> Self {
        OvCell(Cell::new(self.get()))
    }
}

/// Reads a `RefCell` without writing its borrow flag when it is frozen (a frozen object is never borrowed mutably).
#[inline]
pub fn with_ref<T, R>(c: &RefCell<T>, f: impl FnOnce(&T) -> R) -> R {
    if any_frozen() && is_frozen_addr_slow(std::ptr::from_ref(c).addr()) {
        // SAFETY: frozen memory is never written after the freeze, so no mutable borrow exists or can start.
        return f(unsafe { c.try_borrow_unguarded() }
            .expect("shared graph: frozen RefCell borrowed mutably"));
    }
    f(&c.borrow())
}

/// Whether the object at `addr` is frozen (one load while nothing is).
#[inline]
pub fn frozen<T: ?Sized>(r: &T) -> bool {
    any_frozen() && is_frozen_addr_slow(std::ptr::from_ref(r).cast::<u8>().addr())
}

/// A word for a raw pointer kept in an `OvCell` (`OvCell<RawWord>`): the address, provenance exposed.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct RawWord(pub usize);

impl RawWord {
    #[inline]
    pub fn from_ptr<T>(p: *const T) -> RawWord {
        RawWord(p.expose_provenance())
    }
    #[inline]
    pub fn ptr<T>(self) -> *const T {
        std::ptr::with_exposed_provenance(self.0)
    }
}

/// `OptionThinSliceCell` whose writes to a frozen address go to the overlay.
pub struct OvOptionThinSliceCell<T: 'static>(OvCell<Option<crate::ThinSlice<T>>>);

impl<T> OvOptionThinSliceCell<T> {
    #[inline]
    fn new(s: Option<&'static [T]>) -> Self {
        OvOptionThinSliceCell(OvCell::new(s.map(crate::ThinSlice::new)))
    }
    #[inline]
    pub fn get(&self) -> Option<&'static [T]> {
        self.0.get().map(crate::ThinSlice::get)
    }
    #[inline]
    pub fn set(&self, s: Option<&'static [T]>) {
        self.0.set(s.map(crate::ThinSlice::new));
    }
}

impl<T> Default for OvOptionThinSliceCell<T> {
    fn default() -> Self {
        OvOptionThinSliceCell::new(None)
    }
}

const ID_PAGE: usize = 256;

impl Overlay {
    /// A word kept by object id (frozen types' object flags): `None` if never set.
    #[inline]
    pub fn id_word(&self, id: u32) -> Option<u64> {
        let pages = self.id_words.borrow();
        let page = pages.get(id as usize / ID_PAGE)?.as_ref()?;
        let w = page[id as usize % ID_PAGE];
        (w >> 63 != 0).then_some(w & !(1 << 63))
    }

    fn set_id_word(&self, id: u32, value: u64) {
        let mut pages = self.id_words.borrow_mut();
        let p = id as usize / ID_PAGE;
        if pages.len() <= p {
            pages.resize_with(p + 1, || None);
        }
        pages[p].get_or_insert_with(|| Box::new([0; ID_PAGE]))[id as usize % ID_PAGE] =
            value | (1 << 63);
    }
}

/// A frozen type's object flags written by a fork: kept by type id in the fork's overlay; the flags cell's line is
/// marked dirty so reads look there.
pub fn set_id_word_frozen(cell_addr: usize, id: u32, value: u64) {
    mark_dirty(cell_addr);
    current_overlay().set_id_word(id, value);
}

impl<T: Copy + Default> Default for OvExact<T> {
    fn default() -> Self {
        OvExact(Cell::new(T::default()))
    }
}

/// `OvCell` for a word whose unwritten value is not its default (a tag bit set at creation): a frozen cell always
/// looks in the overlay first (one hash lookup per read of a frozen cell).
#[repr(transparent)]
pub struct OvExact<T>(Cell<T>);

impl<T: Copy> OvExact<T> {
    #[inline]
    pub const fn new(v: T) -> Self {
        OvExact(Cell::new(v))
    }
    #[inline]
    pub fn get(&self) -> T {
        let v = self.0.get();
        if !dirty(std::ptr::from_ref(self).addr()) {
            return v;
        }
        overlay_get_frozen(std::ptr::from_ref(self).addr(), v)
    }
    #[inline]
    pub fn set(&self, v: T) {
        if any_frozen() {
            let addr = std::ptr::from_ref(self).addr();
            if is_frozen_addr_slow(addr) {
                current_overlay().set_cell(addr, v);
                return;
            }
        }
        self.0.set(v);
    }
}

#[cold]
#[inline(never)]
fn overlay_get_frozen<T: Copy>(addr: usize, v: T) -> T {
    try_current_overlay().map_or(v, |o| o.get_cell(addr, v))
}

/// `OwnedCell::set` on a frozen cell: kept in the overlay.
#[cold]
#[inline(never)]
pub(crate) fn owned_set_if_frozen<C, T: Copy>(cell: &C, v: T) -> bool {
    let addr = std::ptr::from_ref(cell).addr();
    if !is_frozen_addr_slow(addr) {
        return false;
    }
    current_overlay().set_cell(addr, v);
    true
}

thread_local! {
    static SEED_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Marks the calling thread as the seed checker's: objects it creates get their ids at once (a fork must never
/// write an id into a frozen object).
pub fn set_seed_thread(on: bool) {
    SEED_THREAD.with(|s| s.set(on));
}

#[inline]
pub fn on_seed_thread() -> bool {
    COMPILED_IN && SEED_THREAD.with(Cell::get)
}
