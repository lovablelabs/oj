//! tsrs-only: member lists of unchecked declaration files parsed and bound on first use
//! (notes/mem-lazy-dts-members.md).
//!
//! In a CLI run where no declaration file is type-checked (`skipLibCheck` or `noCheck`), the parser parses the member
//! list of an interface, class or type literal in a declaration file as usual, and if the list is plain (no
//! diagnostic, no eager `JSDoc`, no comment directive, nothing whose binding reaches outside the list, and the arena can
//! discard what the list allocated: `tsrs_parser`'s `parse_member_list_lazily`), it throws the nodes away and keeps a
//! `LazyNodeList` instead: the owner node, where the list starts and the parser context there. The owner's `NodeList`
//! points at the record (a long-form `ThinSlice` whose slice is `LAZY_HEAD_DATA`). The binder skips the list and
//! records its own state at that point (`LazyBindContext`) on the record and on the owner's symbol
//! (`Symbol::set_lazy_list`). The first reader of the list's nodes (`NodeList::nodes`) or of the owner symbol's
//! `members` (or, for a class, `exports`) parses the list again from the recorded position (the parser hook) and binds
//! it with the recorded binder state (the binder hook) on its own thread, then publishes the nodes; every later reader
//! gets the same slice. A declaration that merges into the owner symbol in the same file and fills one of the tables
//! the list fills forces the list while the file is bound, so tables get their entries in source order (`tsrs_binder`
//! `force_pending_lazy_list`).
//!
//! States: `PENDING` (parsed lazily, the binder has not reached it: readers see no members, which only the parser
//! and the binder of the file can observe), `DEFERRED` (the binder skipped it: the next reader parses and binds it),
//! `UNBOUND` (the file was bound without reaching it: the next reader only parses it), `FORCING` (a thread is parsing
//! and binding it: other threads wait, the forcing thread's own reads get the list being bound), `DONE`.
//!
//! Threading: the record's plain fields (`owner`, `bind`) are written by the file's parser and binder before the
//! program is shared; `nodes` is written once by the forcing thread before its Release store of `DONE`, and read by
//! other threads only after an Acquire load of `DONE`.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use tsrs_core::{OwnedCell, P};

use crate::ast::{Node, SourceFile};
use crate::flow::FlowNode;
use crate::nodeflags::NodeFlags;

pub const PENDING: u8 = 0;
pub const DEFERRED: u8 = 1;
const UNBOUND: u8 = 2;
const FORCING: u8 = 3;
const DONE: u8 = 4;
/// `FORCING`, with at least one reader committed to waiting on the record's stripe.
const FORCING_WITH_WAITERS: u8 = 5;

/// `tsrs_parser::ParsingContext::ClassMembers as u8` (asserted there).
pub const CLASS_MEMBERS: u8 = 5;

/// The slice a lazy `NodeList`'s long-form `ThinSlice` reads (`LazyNodeList::head`); its data pointer tells a lazy
/// list from a long one.
static LAZY_HEAD_DATA: [P<Node>; 0] = [];

#[inline]
pub(crate) fn is_lazy_head(s: &'static [P<Node>]) -> bool {
    std::ptr::eq(s.as_ptr(), LAZY_HEAD_DATA.as_ptr())
}

/// The binder state where the binder skipped a lazy member list (`tsrs_binder`).
#[derive(Clone, Copy)]
pub struct LazyBindContext {
    pub container: Option<P<Node>>,
    pub this_container: Option<P<Node>>,
    pub block_scope_container: Option<P<Node>>,
    pub last_container: Option<P<Node>>,
    pub current_flow: Option<P<FlowNode>>,
    pub unreachable_flow: P<FlowNode>,
}

#[repr(C)]
pub struct LazyNodeList {
    /// First, so that a `&LazyNodeList` and the `&'static [P<Node>]` its `NodeList` reads have the same address.
    head: &'static [P<Node>],
    state: AtomicU8,
    /// The parsing context of the list (`tsrs_parser`'s `ParsingContext`: type or class members).
    pub parsing_context: u8,
    /// The forcing thread (`thread_token`).
    forcer: AtomicU64,
    nodes: OwnedCell<&'static [P<Node>]>,
    /// Set right after the parser creates the owner node (the list is parsed first).
    owner: OwnedCell<Option<P<Node>>>,
    /// `pos` / `end` of the list (its `loc`); the parser resumes at `pos`, the end of the `{` token.
    pub pos: u32,
    pub end: u32,
    /// The parser's context flags and enclosing parsing contexts at the list.
    pub context_flags: NodeFlags,
    pub parsing_contexts: u32,
    /// Set by the binder before `DEFERRED`.
    bind: OwnedCell<Option<P<LazyBindContext>>>,
}

/// Parses a lazy list again (the parser hook): returns its members, parents set to the owner.
pub type ParseHook = fn(&LazyNodeList, P<SourceFile>) -> &'static [P<Node>];
/// Binds the members of a lazy list with its recorded binder state (the binder hook).
pub type BindHook = fn(&LazyNodeList, P<SourceFile>, &'static [P<Node>]);

static PARSE_HOOK: OnceLock<ParseHook> = OnceLock::new();
static BIND_HOOK: OnceLock<BindHook> = OnceLock::new();

pub fn set_parse_hook(hook: ParseHook) {
    let _ = PARSE_HOOK.set(hook);
}

pub fn set_bind_hook(hook: BindHook) {
    let _ = BIND_HOOK.set(hook);
}

/// Readers that wait for another thread's force sleep on one of these, picked by the record's address, so that a
/// finished force wakes only the readers of its stripe (one shared condvar woke every sleeping reader at every force:
/// drizzle-orm at 32 checkers waited 166 ms in all for 14 ms of forcing).
static WAIT: [(Mutex<()>, Condvar); 64] = [const { (Mutex::new(()), Condvar::new()) }; 64];
/// A reader spins this many times (a few microseconds) before it sleeps: most lists are parsed and bound again in a few
/// microseconds (formbricks-web: 9,400 lists in 40-80 ms of thread time). Longer spins burned CPU on x86, where a
/// `pause` takes about a hundred cycles.
const SPINS: u32 = 256;

fn wait_stripe(record: &LazyNodeList) -> &'static (Mutex<()>, Condvar) {
    &WAIT[(std::ptr::from_ref(record).addr() >> 6) % WAIT.len()]
}

fn thread_token() -> u64 {
    thread_local! {
        static TOKEN: u8 = const { 0 };
    }
    TOKEN.with(|t| std::ptr::from_ref(t).addr() as u64)
}

impl LazyNodeList {
    pub fn new(
        pos: u32,
        end: u32,
        parsing_context: u8,
        context_flags: NodeFlags,
        parsing_contexts: u32,
    ) -> LazyNodeList {
        LazyNodeList {
            head: &LAZY_HEAD_DATA,
            state: AtomicU8::new(PENDING),
            parsing_context,
            forcer: AtomicU64::new(0),
            nodes: OwnedCell::new(&[]),
            owner: OwnedCell::new(None),
            pos,
            end,
            context_flags,
            parsing_contexts,
            bind: OwnedCell::new(None),
        }
    }

    pub(crate) fn head_ref(&'static self) -> &'static &'static [P<Node>] {
        &self.head
    }

    /// # Safety
    /// `head` is the `head` field of a live `LazyNodeList` (a lazy `NodeList`'s long-form slice).
    pub(crate) unsafe fn from_head(head: &'static &'static [P<Node>]) -> &'static LazyNodeList {
        // SAFETY: `head` is the first field of a `repr(C)` `LazyNodeList` (this function's contract).
        unsafe { tsrs_core::ptr::arena_neighbor(head, 0) }
    }

    /// Whether binding the list adds to the owner symbol's `exports` (static members of a class) as well as to its
    /// `members`.
    #[inline]
    pub fn fills_exports(&self) -> bool {
        self.parsing_context == CLASS_MEMBERS
    }

    #[inline]
    pub fn state(&self) -> u8 {
        // Acquire: pairs with the Release stores of `DONE`, so a caller that sees it may read what the force wrote.
        self.state.load(Ordering::Acquire)
    }

    /// The owner is set once, right after the parser creates the owner node.
    pub fn set_owner(&self, owner: P<Node>) {
        self.owner.set(Some(owner));
    }

    pub fn owner(&self) -> P<Node> {
        self.owner.get().expect("a lazy list's owner")
    }

    /// Binder: the list was skipped in this state; the next reader parses and binds it.
    pub fn defer(&self, ctx: P<LazyBindContext>) {
        self.bind.set(Some(ctx));
        // Release: publishes `bind` with the state (the program is also shared only after binding).
        self.state.store(DEFERRED, Ordering::Release);
    }

    /// Binder, at the end of the file: the binder never reached the list; the next reader only parses it.
    pub fn mark_unbound(&self) {
        // Relaxed: this thread (the file's binder) made every earlier transition of a list still `PENDING`.
        if self.state.load(Ordering::Relaxed) == PENDING {
            // Release: as in `defer`.
            self.state.store(UNBOUND, Ordering::Release);
        }
    }

    pub fn bind_context(&self) -> Option<P<LazyBindContext>> {
        self.bind.get()
    }

    /// Binder (while the file is bound, in a state the record cannot replay): parse the list now; the caller binds it
    /// right where it is, as if it had never been lazy.
    pub fn parse_now(&self, file: P<SourceFile>) -> &'static [P<Node>] {
        let nodes = (PARSE_HOOK.get().expect("lazy list parse hook"))(self, file);
        self.nodes.set(nodes);
        // Release: pairs with the Acquire loads of readers (the program is also shared only after binding).
        self.state.store(DONE, Ordering::Release);
        nodes
    }

    /// The members, parsed (and bound) on first use. In `PENDING` there are none yet (only the file's parser and
    /// binder read the list then).
    #[cold]
    #[inline(never)]
    pub(crate) fn force_nodes(&self) -> &'static [P<Node>] {
        loop {
            // Acquire: pairs with the Release store of `DONE`, which publishes `nodes` and what the binder wrote.
            match self.state.load(Ordering::Acquire) {
                DONE => return self.nodes.get(),
                PENDING => return &[],
                s @ (DEFERRED | UNBOUND) => {
                    // Acquire: as above; the record's `bind` was published with `DEFERRED`.
                    if self
                        .state
                        .compare_exchange(s, FORCING, Ordering::Acquire, Ordering::Acquire)
                        .is_ok()
                    {
                        // Relaxed: read back only by this thread; another thread only needs to see a different value.
                        self.forcer.store(thread_token(), Ordering::Relaxed);
                        return self.force(s == DEFERRED);
                    }
                }
                _ => {
                    // Relaxed: the forcing thread stores its token right after winning the exchange, on its own
                    // thread; any other value means another thread, which this one waits for.
                    if self.forcer.load(Ordering::Relaxed) == thread_token() {
                        return self.nodes.get();
                    }
                    // Most lists take microseconds to parse and bind: spin before sleeping on the condvar.
                    let mut spins = 0;
                    // Acquire: as above.
                    while self.state.load(Ordering::Acquire) == FORCING && spins < SPINS {
                        std::hint::spin_loop();
                        spins += 1;
                    }
                    // Acquire on success: as for the loads above. On failure it makes a concurrent `DONE` and the
                    // nodes it publishes visible before the check under the stripe lock. The forcing thread swaps
                    // the state to `DONE`, so it either sees this registration and notifies, or wins the race and
                    // this exchange fails without a reader going to sleep.
                    let _ = self.state.compare_exchange(
                        FORCING,
                        FORCING_WITH_WAITERS,
                        // The state value itself registers the waiter; there is no other data to release.
                        Ordering::Acquire,
                        // A failed exchange may observe `DONE` and must acquire the published nodes.
                        Ordering::Acquire,
                    );
                    let (lock, cv) = wait_stripe(self);
                    // Waits for the forcing thread without holding a run slot (`tsrs_core::runslots`).
                    tsrs_core::runslots::blocking(|| {
                        let mut g = lock.lock().unwrap();
                        // Acquire: as above.
                        while matches!(
                            self.state.load(Ordering::Acquire),
                            FORCING | FORCING_WITH_WAITERS
                        ) {
                            g = cv.wait(g).unwrap();
                        }
                    });
                }
            }
        }
    }

    fn force(&self, bind: bool) -> &'static [P<Node>] {
        let file = crate::utilities_1::get_source_file_of_node(Some(self.owner()))
            .expect("a lazy list's file");
        // The reader may be inside a scratch region or another allocation scope: the tree is shared and permanent.
        let _escape = tsrs_core::arena::escape_scratch();
        let _arena = tsrs_core::arena::enter_thread_arena();
        let nodes = (PARSE_HOOK.get().expect("lazy list parse hook"))(self, file);
        // Set before binding: the binder's own reads of the list (re-entrant, `FORCING` by this thread) see it.
        self.nodes.set(nodes);
        if bind {
            (BIND_HOOK.get().expect("lazy list bind hook"))(self, file, nodes);
        }
        // Release: publishes `nodes` and everything the parser and binder wrote to readers that load `DONE`. The
        // read half observes a waiter's registration in the same atomic modification order. A std condvar notify is
        // a futex system call on Linux even with nobody waiting, and most lists have no competing reader.
        let waiting = self.state.swap(DONE, Ordering::Release) == FORCING_WITH_WAITERS;
        if waiting {
            let (lock, cv) = wait_stripe(self);
            // Prevent a missed wake: a registered reader either sees `DONE` while it holds the lock, or releases the
            // lock atomically as it sleeps before this thread acquires it.
            drop(lock.lock().unwrap());
            cv.notify_all();
        }
        nodes
    }

    /// Forces the list unless it is done (`Symbol::members` / `exports` of the owner).
    #[inline]
    pub fn ensure(&self) {
        // Acquire: as in `state`.
        if self.state.load(Ordering::Acquire) != DONE {
            self.force_nodes();
        }
    }

    /// Binder (while the file is bound): a declaration that fills one of the owner symbol's tables merges into it,
    /// so the deferred list is parsed and bound by `bind` now, before the merge.
    pub fn force_inline(&self, file: P<SourceFile>, bind: &mut dyn FnMut(&'static [P<Node>])) {
        // Acquire: as in `force_nodes` (the binder of this file made the earlier transitions, on this thread).
        if self
            .state
            .compare_exchange(DEFERRED, FORCING, Ordering::Acquire, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        // Relaxed: as in `force_nodes`.
        self.forcer.store(thread_token(), Ordering::Relaxed);
        let nodes = (PARSE_HOOK.get().expect("lazy list parse hook"))(self, file);
        self.nodes.set(nodes);
        bind(nodes);
        // Release: as in `force`.
        self.state.store(DONE, Ordering::Release);
    }
}
