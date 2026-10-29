//! `FrozenCell<T>`: interior mutability for arena objects that are shared between checker threads.
//!
//! Go's AST nodes and binder symbols are written while a file is parsed and bound and are then read
//! concurrently by every checker, with no synchronization: they are frozen by convention. `RefCell`
//! cannot express that, because its borrow counter is written even by `borrow()`, so concurrent readers
//! race on it. `FrozenCell` has the same `borrow()` / `borrow_mut()` surface without a runtime flag.
//!
//! Its one user is `SymbolTable` (`tsrs_ast/src/symbol.rs`). Shared data that is still written after binding
//! does not use it: a `SourceFile`'s `JSDoc` cache is a `OnceLock` map (files without lazy `JSDoc`) or an
//! `RwLock`ed map (files with it), and `TsConfigSourceFile` keeps its two filled-in fields in `Mutex`es (#646).
//!
//! Contract (see "Threading" in docs/PORTING.md):
//! - `borrow_mut()` is only called by the thread that owns the object: the binder while the file is being
//!   bound, or a checker on tables it created itself. Once a file is bound, its binder symbol tables are
//!   never written again.
//! - A `borrow_mut()` guard is never held across a call that may read or write the same cell (same rule
//!   as for `RefCell`).
//!
//! Builds with `debug_assertions` or the `checked-cells` feature keep an atomic borrow counter and panic
//! on aliasing violations, like `RefCell` but without a data race; release builds do no bookkeeping.

use std::cell::{Cell, UnsafeCell};
use std::fmt;
use std::ops::{Deref, DerefMut};

#[cfg(any(debug_assertions, feature = "checked-cells"))]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(any(debug_assertions, feature = "checked-cells"))]
const EXCLUSIVE_BORROW: usize = usize::MAX;

pub struct FrozenCell<T> {
    value: UnsafeCell<T>,
    #[cfg(any(debug_assertions, feature = "checked-cells"))]
    state: AtomicUsize, // 1..EXCLUSIVE_BORROW: shared borrow count; EXCLUSIVE_BORROW: exclusive borrow
}

// SAFETY: shared access is read-only except through `borrow_mut`, whose callers follow the ownership
// contract in the module docs (only the owning thread writes, and never while other threads can read).
unsafe impl<T: Send + Sync> Sync for FrozenCell<T> {}
// SAFETY: the cell owns its `T` like an `UnsafeCell<T>`, so it is `Send` when `T` is.
unsafe impl<T: Send> Send for FrozenCell<T> {}

impl<T> FrozenCell<T> {
    #[inline]
    pub const fn new(value: T) -> FrozenCell<T> {
        FrozenCell {
            value: UnsafeCell::new(value),
            #[cfg(any(debug_assertions, feature = "checked-cells"))]
            state: AtomicUsize::new(0),
        }
    }

    #[inline]
    pub fn borrow(&self) -> FrozenRef<'_, T> {
        #[cfg(any(debug_assertions, feature = "checked-cells"))]
        {
            let mut state = self.state.load(Ordering::Acquire);
            loop {
                assert!(state != EXCLUSIVE_BORROW, "FrozenCell already mutably borrowed");
                assert!(state < EXCLUSIVE_BORROW - 1, "FrozenCell shared borrow count overflow");
                state = match self.state.compare_exchange_weak(
                    state,
                    state + 1,
                    Ordering::Acquire,
                    // A failed comparison only supplies the next borrow-state value to retry.
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(current) => current,
                };
            }
        }
        FrozenRef {
            // SAFETY: see the module contract; no exclusive borrow is live.
            value: unsafe { &*self.value.get() },
            #[cfg(any(debug_assertions, feature = "checked-cells"))]
            state: &self.state,
        }
    }

    #[inline]
    pub fn borrow_mut(&self) -> FrozenRefMut<'_, T> {
        #[cfg(any(debug_assertions, feature = "checked-cells"))]
        {
            crate::ptr::shared_check::assert_not_shared(self, "FrozenCell");
            if self
                .state
                // Failure only reports that another borrow is active; it does not expose the value.
                .compare_exchange(0, EXCLUSIVE_BORROW, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                panic!("FrozenCell already borrowed");
            }
        }
        FrozenRefMut {
            // SAFETY: see the module contract; only the owning thread writes, and no other borrow is live.
            value: unsafe { &mut *self.value.get() },
            #[cfg(any(debug_assertions, feature = "checked-cells"))]
            state: &self.state,
        }
    }

    /// Replaces the value (owner thread only, like `borrow_mut`).
    #[inline]
    pub fn replace(&self, value: T) -> T {
        std::mem::replace(&mut *self.borrow_mut(), value)
    }
}

impl<T: Default> Default for FrozenCell<T> {
    #[inline]
    fn default() -> Self {
        FrozenCell::new(T::default())
    }
}

impl<T: fmt::Debug> fmt::Debug for FrozenCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FrozenCell").field(&*self.borrow()).finish()
    }
}

pub struct FrozenRef<'a, T> {
    value: &'a T,
    #[cfg(any(debug_assertions, feature = "checked-cells"))]
    state: &'a AtomicUsize,
}

impl<T> Deref for FrozenRef<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        self.value
    }
}

#[cfg(any(debug_assertions, feature = "checked-cells"))]
impl<T> Drop for FrozenRef<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.state.fetch_sub(1, Ordering::Release);
    }
}

pub struct FrozenRefMut<'a, T> {
    value: &'a mut T,
    #[cfg(any(debug_assertions, feature = "checked-cells"))]
    state: &'a AtomicUsize,
}

impl<T> Deref for FrozenRefMut<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T> DerefMut for FrozenRefMut<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        self.value
    }
}

#[cfg(any(debug_assertions, feature = "checked-cells"))]
impl<T> Drop for FrozenRefMut<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.state.store(0, Ordering::Release);
    }
}

/// `Cell<T>` for fields of shared arena objects (binder symbols) that only their owner writes: after binding,
/// checkers write these fields only on objects they created themselves (transient symbols).
///
/// Checked builds verify that with `shared_check` (`TSRS_CHECK_SHARED=1`); otherwise it is a plain `Cell`.
#[repr(transparent)]
#[derive(Default)]
pub struct OwnedCell<T>(Cell<T>);

impl<T> OwnedCell<T> {
    #[inline]
    pub const fn new(value: T) -> OwnedCell<T> {
        OwnedCell(Cell::new(value))
    }
}

// Shared graph (`sharedgraph`): a checker-created symbol or node of the frozen seed is read through the
// current checker's overlay, and its writes go there.
impl<T: Copy> OwnedCell<T> {
    #[inline]
    pub fn set(&self, value: T) {
        crate::ptr::shared_check::assert_not_shared(self, "OwnedCell");
        if crate::sharedgraph::any_frozen() && crate::sharedgraph::owned_set_if_frozen(self, value)
        {
            return;
        }
        self.0.set(value);
    }

    #[inline]
    pub fn get(&self) -> T {
        self.0.get()
    }
}

/// Read access for helpers written against `&Cell<T>` (they must not write through it after binding).
impl<T> std::ops::Deref for OwnedCell<T> {
    type Target = Cell<T>;
    #[inline]
    fn deref(&self) -> &Cell<T> {
        &self.0
    }
}

impl<T: Copy + fmt::Debug> fmt::Debug for OwnedCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.get().fmt(f)
    }
}

/// `OwnedCell<&'static str>` in 8 bytes with two tag bits for the owner (`tags` / `set_tags`; `set` keeps
/// them): the address in the low 48 bits, the length in the next 14, the tags in the top 2.
///
/// A string of `0x3FFF` bytes or more (or at an address above 2^48) is copied into the arena after a `u32`
/// length (`PackedStr`'s long form).
pub struct OwnedTaggedStrCell(Cell<u64>);

const TAGGED_LEN_SHIFT: u32 = 48;
const TAGGED_LEN_MASK: u64 = (1 << 14) - 1;
const TAGGED_TAG_SHIFT: u32 = 62;
const TAGGED_ADDR: u64 = (1 << TAGGED_LEN_SHIFT) - 1;

impl OwnedTaggedStrCell {
    #[inline]
    pub fn new(value: &'static str) -> OwnedTaggedStrCell {
        OwnedTaggedStrCell(Cell::new(Self::pack(value)))
    }

    #[inline]
    fn pack(s: &'static str) -> u64 {
        let a = s.as_ptr().expose_provenance() as u64;
        if (s.len() as u64) < TAGGED_LEN_MASK && a >> TAGGED_LEN_SHIFT == 0 {
            return a | ((s.len() as u64) << TAGGED_LEN_SHIFT);
        }
        Self::pack_long(s)
    }

    #[cold]
    fn pack_long(s: &str) -> u64 {
        let len = u32::try_from(s.len()).expect("string longer than u32::MAX");
        let mut bytes = Vec::with_capacity(4 + s.len());
        bytes.extend_from_slice(&len.to_ne_bytes());
        bytes.extend_from_slice(s.as_bytes());
        let copy = crate::alloc_slice_aligned4(&bytes);
        let a = copy.as_ptr().expose_provenance() as u64;
        assert!(a >> TAGGED_LEN_SHIFT == 0, "arena address above 2^48");
        a | (TAGGED_LEN_MASK << TAGGED_LEN_SHIFT)
    }

    #[inline]
    #[expect(
        clippy::disallowed_methods,
        reason = "from_utf8 here and in PackedStr::as_str: +5% instructions, one checker (notes/lint-paydown-compiler.md)"
    )]
    pub fn get(&self) -> &'static str {
        let w = self.0.get();
        debug_assert_ne!(w, 0, "resolve the shared string before reading it");
        let p = std::ptr::with_exposed_provenance::<u8>((w & TAGGED_ADDR) as usize);
        let len = (w >> TAGGED_LEN_SHIFT) & TAGGED_LEN_MASK;
        // SAFETY: built by `pack` from a `&'static str` of this length, or by `pack_long` (length prefix + bytes).
        unsafe {
            let (p, len) = if len == TAGGED_LEN_MASK {
                (p.add(4), p.cast::<u32>().read_unaligned() as usize)
            } else {
                (p, len as usize)
            };
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(p, len))
        }
    }

    #[inline]
    pub fn set(&self, value: &'static str) {
        crate::ptr::shared_check::assert_not_shared(self, "OwnedTaggedStrCell");
        self.0.set(
            Self::pack(value)
                | self.0.get() & !(TAGGED_ADDR | (TAGGED_LEN_MASK << TAGGED_LEN_SHIFT)),
        );
    }

    /// The two tag bits.
    #[inline]
    pub fn tags(&self) -> u8 {
        (self.0.get() >> TAGGED_TAG_SHIFT) as u8
    }

    #[inline]
    pub fn set_tags(&self, tags: u8) {
        crate::ptr::shared_check::assert_not_shared(self, "OwnedTaggedStrCell");
        debug_assert!(tags < 4);
        self.0.set(self.0.get() & !(3 << TAGGED_TAG_SHIFT) | ((tags as u64) << TAGGED_TAG_SHIFT));
    }
}

impl Default for OwnedTaggedStrCell {
    fn default() -> Self {
        OwnedTaggedStrCell::new("")
    }
}
