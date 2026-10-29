use std::cell::Cell;
use std::collections::hash_map::Entry;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Once, OnceLock, RwLock};

use rustc_hash::FxHashMap;
use tsrs_core::collections::Set;
use tsrs_core::tspath::Path;
use tsrs_core::{
    LanguageVariant, OwnedCell, P, ResolutionMode, ScriptKind, TextPos, TextRange, ThinSlice,
    Tristate, alloc_slice, alloc_str, alloc_vec, compute_ecma_line_starts, undefined_text_range,
};
use tsrs_diagnostics as diagnostics;
use tsrs_spanmap::SpanMap;

use crate::*;

// parseJSDocForNode is the package-level function for lazily parsing JSDoc.
// It is set by the parser crate via set_parse_jsdoc_for_node.
pub type ParseJSDocForNodeFn = fn(&'static SourceFile, P<Node>) -> Vec<P<Node>>;

static PARSE_JSDOC_FOR_NODE: OnceLock<ParseJSDocForNodeFn> = OnceLock::new();

// SetParseJSDocForNode registers the lazy JSDoc parse function. Called once by the parser crate.
pub fn set_parse_jsdoc_for_node(f: ParseJSDocForNodeFn) {
    let _ = PARSE_JSDOC_FOR_NODE.set(f);
}

// Visitor
//
// Go `type Visitor func(*Node) bool`; Rust visitors are `&mut dyn FnMut(P<Node>) -> bool`.

#[inline]
pub(crate) fn visit<V: FnMut(P<Node>) -> bool + ?Sized>(v: &mut V, node: Option<P<Node>>) -> bool {
    match node {
        Some(node) => v(node),
        None => false,
    }
}

#[inline]
pub(crate) fn visit_nodes<V: FnMut(P<Node>) -> bool + ?Sized>(
    v: &mut V,
    nodes: &[P<Node>],
) -> bool {
    nodes.iter().any(|&node| v(node))
}

#[inline]
pub(crate) fn visit_node_list<V: FnMut(P<Node>) -> bool + ?Sized>(
    v: &mut V,
    node_list: Option<P<NodeList>>,
) -> bool {
    match node_list {
        Some(list) => visit_nodes(v, list.nodes()),
        None => false,
    }
}

#[inline]
pub(crate) fn visit_modifiers<V: FnMut(P<Node>) -> bool + ?Sized>(
    v: &mut V,
    modifiers: Option<P<ModifierList>>,
) -> bool {
    match modifiers {
        Some(list) => visit_nodes(v, list.list.nodes()),
        None => false,
    }
}

/// Go `core.Same` on slices: identity of the backing array and length.
#[inline]
pub(crate) fn same_slice<T>(a: &[T], b: &[T]) -> bool {
    a.len() == b.len() && std::ptr::eq(a.as_ptr(), b.as_ptr())
}

// NodeFactory

pub type NodeHook = Rc<dyn Fn(P<Node>)>;
pub type NodeUpdateHook = Rc<dyn Fn(P<Node>, P<Node>)>;

#[derive(Default, Clone)]
pub struct NodeFactoryHooks {
    pub on_create: Option<NodeHook>, // Hooks the creation of a node.
    pub on_update: Option<NodeUpdateHook>, // Hooks the updating of a node.
    pub on_clone: Option<NodeUpdateHook>, // Hooks the cloning of a node.
}

/// Go `*ast.NodeFactory`.
///
/// A handle: every method takes `&self`, and `clone()` yields another handle to the same factory (shared
/// hooks and counters), like copying the Go pointer. So `f.new_x(f.new_y())` works and a factory can be
/// shared by a visitor, the node builder and an emit context.
#[derive(Clone)]
pub struct NodeFactory {
    pub(crate) hooks: NodeFactoryHooks,
    node_count: Rc<Cell<usize>>,
    pub(crate) text_count: Rc<Cell<usize>>,
    /// tsrs-only: nodes and lists go to the thread's scratch region when one is entered (`P::new_scratch`), also
    /// while the checker has escaped from it: the factory of a per-file emit context, whose nodes die with the file
    /// (notes/mem-emit-regions.md).
    scratch: bool,
    /// The common allocation path has neither a create hook nor scratch routing. Keeping that fact beside the
    /// factory folds the allocation-route and create-hook tests into one branch for every node it creates.
    plain_create: bool,
}

impl Default for NodeFactory {
    fn default() -> Self {
        new_node_factory(NodeFactoryHooks::default())
    }
}

pub fn new_node_factory(hooks: NodeFactoryHooks) -> NodeFactory {
    new_node_factory_ex(hooks, false)
}

/// `new_node_factory`; `scratch`: see `NodeFactory::scratch`.
pub fn new_node_factory_ex(hooks: NodeFactoryHooks, scratch: bool) -> NodeFactory {
    let plain_create = !scratch && hooks.on_create.is_none();
    NodeFactory {
        hooks,
        node_count: Rc::default(),
        text_count: Rc::default(),
        scratch,
        plain_create,
    }
}

fn node_header(kind: Kind, data_tag: NodeDataTag) -> Node {
    Node {
        header: OwnedCell::new(NodeHeaderWord::new(kind, data_tag)),
        flags: OwnedCell::new(NodeFlags::None),
        id: AtomicU32::new(0),
        loc: OwnedCell::new(undefined_text_range()),
    }
}

/// Creates a node whose data struct is `data` (Go: the data struct embeds `NodeBase`, one allocation).
pub(crate) fn new_node<T: NodePayload>(kind: Kind, data: T, hooks: &NodeFactoryHooks) -> P<Node> {
    new_node_hooked(kind, data, hooks)
}

#[inline]
fn new_node_plain<T: NodePayload>(kind: Kind, data: T) -> P<Node> {
    // SAFETY: `NodeAlloc` is `repr(C)` with the header first, so the pointer to the allocation is a pointer to
    // its header; the header is never moved or freed (leak arena).
    unsafe { P::new(NodeAlloc { node: node_header(kind, T::TAG), data }).cast::<Node>() }
}

#[inline]
fn new_node_hooked<T: NodePayload>(kind: Kind, data: T, hooks: &NodeFactoryHooks) -> P<Node> {
    // SAFETY: `NodeAlloc` is `repr(C)` with the header first, so the pointer to the allocation is a pointer to
    // its header; the header is never moved or freed (leak arena).
    let n = unsafe { P::new(NodeAlloc { node: node_header(kind, T::TAG), data }).cast::<Node>() };
    if let Some(on_create) = &hooks.on_create {
        on_create(n);
    }
    n
}

#[inline]
fn new_node_scratch<T: NodePayload>(kind: Kind, data: T, hooks: &NodeFactoryHooks) -> P<Node> {
    // SAFETY: as in `new_node_hooked`, but this allocation is owned by the active scratch region.
    let n = unsafe {
        P::new_scratch(NodeAlloc { node: node_header(kind, T::TAG), data }).cast::<Node>()
    };
    if let Some(on_create) = &hooks.on_create {
        on_create(n);
    }
    n
}

/// Creates a node whose data struct `data` is followed by its rare tail `rare` (`NodeAllocRare`; the header's
/// rare bit says the tail is there). The factory uses it when one of the struct's rare fields is set.
fn new_node_with_rare_hooked<T: NodeRareTail>(
    kind: Kind,
    data: T,
    rare: T::Rare,
    hooks: &NodeFactoryHooks,
) -> P<Node> {
    let () = T::SAME_OFFSET;
    let mut header = node_header(kind, T::TAG);
    header.header = OwnedCell::new(header.header.get().with_rare_tail());
    // SAFETY: as in `new_node` (`NodeAllocRare` is `repr(C)` with the header first).
    let n = unsafe { P::new(NodeAllocRare { node: header, data, rare }).cast::<Node>() };
    if let Some(on_create) = &hooks.on_create {
        on_create(n);
    }
    n
}

#[inline]
fn new_node_with_rare_scratch<T: NodeRareTail>(
    kind: Kind,
    data: T,
    rare: T::Rare,
    hooks: &NodeFactoryHooks,
) -> P<Node> {
    let () = T::SAME_OFFSET;
    let mut header = node_header(kind, T::TAG);
    header.header = OwnedCell::new(header.header.get().with_rare_tail());
    // SAFETY: as in `new_node_with_rare_hooked`, but this allocation is owned by the active scratch region.
    let n = unsafe { P::new_scratch(NodeAllocRare { node: header, data, rare }).cast::<Node>() };
    if let Some(on_create) = &hooks.on_create {
        on_create(n);
    }
    n
}

#[inline]
fn new_node_with_rare_plain<T: NodeRareTail>(kind: Kind, data: T, rare: T::Rare) -> P<Node> {
    let () = T::SAME_OFFSET;
    let mut header = node_header(kind, T::TAG);
    header.header = OwnedCell::new(header.header.get().with_rare_tail());
    // SAFETY: as in `new_node_plain` (`NodeAllocRare` is `repr(C)` with the header first).
    unsafe { P::new(NodeAllocRare { node: header, data, rare }).cast::<Node>() }
}

/// The rare tail of the node whose data struct is `data`, if it was allocated with one.
#[inline]
pub(crate) fn rare_tail<T: NodeRareTail>(data: &T) -> Option<&'static T::Rare> {
    let () = T::SAME_OFFSET;
    // SAFETY: data structs are created only inside a `NodeAlloc<T>` or `NodeAllocRare<T, _>` (both `repr(C)`, header
    // first, data at the same offset: asserted in `NodeRareTail`), which is never moved or freed while reachable.
    let node: &Node = unsafe {
        tsrs_core::ptr::arena_neighbor(data, -(std::mem::offset_of!(NodeAlloc<T>, data) as isize))
    };
    if !node.header.get().has_rare_tail() {
        return None;
    }
    let off = std::mem::offset_of!(NodeAllocRare<T, T::Rare>, rare)
        - std::mem::offset_of!(NodeAllocRare<T, T::Rare>, data);
    // SAFETY: the rare bit is set only by `new_node_with_rare`, which allocated the tail at this offset.
    Some(unsafe { tsrs_core::ptr::arena_neighbor(data, off as isize) })
}

/// Creates a node whose data struct has no fields (`Token`, `KeywordTypeNode`, ...): just the header.
fn new_empty_node_hooked(kind: Kind, data_tag: NodeDataTag, hooks: &NodeFactoryHooks) -> P<Node> {
    let n = P::new(node_header(kind, data_tag));
    if let Some(on_create) = &hooks.on_create {
        on_create(n);
    }
    n
}

#[inline]
fn new_empty_node_scratch(kind: Kind, data_tag: NodeDataTag, hooks: &NodeFactoryHooks) -> P<Node> {
    let n = P::new_scratch(node_header(kind, data_tag));
    if let Some(on_create) = &hooks.on_create {
        on_create(n);
    }
    n
}

#[inline]
fn new_empty_node_plain(kind: Kind, data_tag: NodeDataTag) -> P<Node> {
    P::new(node_header(kind, data_tag))
}

impl NodeFactory {
    pub fn new(hooks: NodeFactoryHooks) -> NodeFactory {
        new_node_factory(hooks)
    }

    #[inline]
    pub(crate) fn new_node<T: NodePayload>(&self, kind: Kind, data: T) -> P<Node> {
        self.node_count.set(self.node_count.get() + 1);
        if self.plain_create {
            new_node_plain(kind, data)
        } else if self.scratch {
            new_node_scratch(kind, data, &self.hooks)
        } else {
            new_node_hooked(kind, data, &self.hooks)
        }
    }

    #[inline]
    pub(crate) fn new_node_with_rare<T: NodeRareTail>(
        &self,
        kind: Kind,
        data: T,
        rare: T::Rare,
    ) -> P<Node> {
        self.node_count.set(self.node_count.get() + 1);
        if self.plain_create {
            new_node_with_rare_plain(kind, data, rare)
        } else if self.scratch {
            new_node_with_rare_scratch(kind, data, rare, &self.hooks)
        } else {
            new_node_with_rare_hooked(kind, data, rare, &self.hooks)
        }
    }

    #[inline]
    pub(crate) fn new_empty_node(&self, kind: Kind, data_tag: NodeDataTag) -> P<Node> {
        self.node_count.set(self.node_count.get() + 1);
        if self.plain_create {
            new_empty_node_plain(kind, data_tag)
        } else if self.scratch {
            new_empty_node_scratch(kind, data_tag, &self.hooks)
        } else {
            new_empty_node_hooked(kind, data_tag, &self.hooks)
        }
    }

    pub fn node_count(&self) -> usize {
        self.node_count.get()
    }

    pub fn text_count(&self) -> usize {
        self.text_count.get()
    }

    /// Whether this factory allocates in the scratch region (see `NodeFactory::scratch`).
    #[inline]
    pub fn is_scratch(&self) -> bool {
        self.scratch
    }

    /// Copies `s` for a node this factory creates (identifier and literal text): into the scratch region for a
    /// scratch factory, like the node.
    #[inline]
    pub fn alloc_text(&self, s: &str) -> &'static str {
        if self.scratch { tsrs_core::alloc_str_scratch(s) } else { alloc_str(s) }
    }

    #[inline]
    fn alloc_nodes_vec(&self, nodes: Vec<P<Node>>) -> &'static [P<Node>] {
        if self.scratch { tsrs_core::alloc_vec_scratch(nodes) } else { alloc_vec(nodes) }
    }

    #[inline]
    fn alloc_nodes_slice(&self, nodes: &[P<Node>]) -> &'static [P<Node>] {
        if self.scratch { tsrs_core::alloc_slice_scratch(nodes) } else { alloc_slice(nodes) }
    }
}

pub(crate) fn update_node(
    updated: P<Node>,
    original: P<Node>,
    hooks: &NodeFactoryHooks,
) -> P<Node> {
    if updated != original {
        updated.flags.set(original.flags.get());
        updated.loc.set(original.loc.get());
        if let Some(on_update) = &hooks.on_update {
            on_update(updated, original);
        }
    }
    updated
}

pub(crate) fn clone_node(updated: P<Node>, original: P<Node>, hooks: &NodeFactoryHooks) -> P<Node> {
    update_node(updated, original, hooks);
    if updated != original
        && let Some(on_clone) = &hooks.on_clone
    {
        on_clone(updated, original);
    }
    updated
}

// NodeList

/// `nodes` is a `ThinSlice` (pointer and length in one word): 16 bytes, 4.2M lists on the private monorepo.
pub struct NodeList {
    pub loc: OwnedCell<TextRange>,
    nodes: ThinSlice<P<Node>>,
}

const _: () = assert!(std::mem::size_of::<NodeList>() == 16);

impl NodeFactory {
    pub fn new_node_list(&self, nodes: Vec<P<Node>>) -> P<NodeList> {
        self.new_node_list_from_static(self.alloc_nodes_vec(nodes))
    }

    pub fn new_node_list_from_slice(&self, nodes: &[P<Node>]) -> P<NodeList> {
        self.new_node_list_from_static(self.alloc_nodes_slice(nodes))
    }

    /// Stores `nodes` without copying (keeps slice identity, e.g. for sentinel slices).
    pub fn new_node_list_from_static(&self, nodes: &'static [P<Node>]) -> P<NodeList> {
        P::new_in(self.scratch, NodeList::new(undefined_text_range(), nodes))
    }

    /// A member list parsed on first use (`lazylist`).
    pub fn new_lazy_node_list(
        &self,
        loc: TextRange,
        record: &'static crate::lazylist::LazyNodeList,
    ) -> P<NodeList> {
        P::new_in(self.scratch, NodeList::new_lazy(loc, record))
    }
}

impl NodeList {
    /// A list value (callers allocate it with `P::new`); `nodes()` returns `nodes` (same slice).
    #[inline]
    pub fn new(loc: TextRange, nodes: &'static [P<Node>]) -> NodeList {
        NodeList { loc: OwnedCell::new(loc), nodes: ThinSlice::new(nodes) }
    }
    #[inline]
    pub fn nodes(&self) -> &'static [P<Node>] {
        if self.nodes.is_long() {
            return self.nodes_long();
        }
        self.nodes.get()
    }

    /// A list of 2^16 nodes or more, or a lazily parsed member list (`lazylist`).
    #[cold]
    #[inline(never)]
    fn nodes_long(&self) -> &'static [P<Node>] {
        match self.lazy_record() {
            Some(record) => record.force_nodes(),
            None => self.nodes.get(),
        }
    }

    /// A list whose members are parsed on first use (`lazylist`), with its loc.
    fn new_lazy(loc: TextRange, record: &'static crate::lazylist::LazyNodeList) -> NodeList {
        NodeList { loc: OwnedCell::new(loc), nodes: ThinSlice::from_ref(record.head_ref()) }
    }

    /// The lazy-list record of a list made by `new_lazy` (forced or not).
    #[inline]
    pub fn lazy_record(&self) -> Option<&'static crate::lazylist::LazyNodeList> {
        let head = self.nodes.long_ref()?;
        // SAFETY: a long-form slice whose data is the lazy head was made by `new_lazy` from a record's `head`.
        crate::lazylist::is_lazy_head(head)
            .then(|| unsafe { crate::lazylist::LazyNodeList::from_head(head) })
    }
    #[inline]
    pub fn loc(&self) -> TextRange {
        self.loc.get()
    }
    #[inline]
    pub fn pos(&self) -> TextPos {
        self.loc.get().pos()
    }
    #[inline]
    pub fn end(&self) -> TextPos {
        self.loc.get().end()
    }

    pub fn has_trailing_comma(&self) -> bool {
        let Some(last) = self.nodes().last() else {
            return false;
        };
        last.has_trailing_comma_marker() || last.end() < self.end()
    }

    pub fn clone_list(&self, f: &NodeFactory) -> P<NodeList> {
        let result = f.new_node_list_from_static(self.nodes());
        result.loc.set(self.loc.get());
        result
    }
}

// ModifierList

pub struct ModifierList {
    pub list: NodeList,
    pub modifier_flags: ModifierFlags,
}

impl NodeFactory {
    pub fn new_modifier_list(&self, nodes: Vec<P<Node>>) -> P<ModifierList> {
        self.new_modifier_list_from_static(self.alloc_nodes_vec(nodes))
    }

    pub fn new_modifier_list_from_slice(&self, nodes: &[P<Node>]) -> P<ModifierList> {
        self.new_modifier_list_from_static(self.alloc_nodes_slice(nodes))
    }

    fn new_modifier_list_from_static(&self, nodes: &'static [P<Node>]) -> P<ModifierList> {
        P::new_in(
            self.scratch,
            ModifierList {
                list: NodeList::new(undefined_text_range(), nodes),
                modifier_flags: modifiers_to_flags(nodes),
            },
        )
    }
}

impl ModifierList {
    #[inline]
    pub fn nodes(&self) -> &'static [P<Node>] {
        self.list.nodes()
    }
    #[inline]
    pub fn loc(&self) -> TextRange {
        self.list.loc.get()
    }
    #[inline]
    pub fn pos(&self) -> TextPos {
        self.list.pos()
    }
    #[inline]
    pub fn end(&self) -> TextPos {
        self.list.end()
    }
    #[inline]
    pub(crate) fn has_trailing_comma(&self) -> bool {
        self.list.has_trailing_comma()
    }

    pub fn clone_list(&self, f: &NodeFactory) -> P<ModifierList> {
        P::new_in(
            f.scratch,
            ModifierList {
                list: NodeList::new(self.list.loc.get(), self.list.nodes()),
                modifier_flags: self.modifier_flags,
            },
        )
    }
}

// AST Node

/// Go `ast.Node`.
///
/// The node's data struct (Go: the struct that embeds `NodeBase`) is allocated together with the header,
/// right after it (`NodeAlloc`); `data_tag()` says which struct it is. `node.data()` returns it as a
/// `NodeData`, `as_*()` and the generated accessors read it in place. Data structs without fields (`Token`,
/// `KeywordTypeNode`, ...) allocate only the header.
///
/// The header is 24 bytes: the kind, the data tag and the parent pointer share one word (`NodeHeaderWord`),
/// and the id is stored in 32 bits (ordinary ids are 31-bit; the high bit tags ids from a checker's reserved
/// 64-bit block range, decoded by `get_node_id`).
pub struct Node {
    header: OwnedCell<NodeHeaderWord>,
    pub flags: OwnedCell<NodeFlags>,
    pub(crate) id: AtomicU32,
    pub(crate) loc: OwnedCell<TextRange>, // set_loc() (compact identifiers derive their text from the end)
}

const _: () = assert!(std::mem::size_of::<Node>() == 24);

/// A node's kind, data tag and parent in one word: the parent in the low 45 bits (`P::pack`), the kind in the next 9 bits, the data
/// tag in the 8 above, the rare bit (the data struct is followed by its rare tail, `NodeRareTail`) and the
/// trailing-comma bit. The latter replaces the old `-2` synthetic-position sentinel. Only the parent and
/// trailing-comma bits change after creation.
#[derive(Clone, Copy)]
struct NodeHeaderWord(u64);

impl NodeHeaderWord {
    const PARENT_BITS: u32 = tsrs_core::PACK_BITS;
    const PARENT_MASK: u64 = (1 << Self::PARENT_BITS) - 1;
    const KIND_SHIFT: u32 = Self::PARENT_BITS;
    const KIND_BITS: u32 = 9;
    const TAG_SHIFT: u32 = Self::KIND_SHIFT + Self::KIND_BITS;
    const RARE_BIT: u64 = 1 << (Self::TAG_SHIFT + 8);
    const TRAILING_COMMA_BIT: u64 = 1 << 63;

    #[inline]
    fn new(kind: Kind, data_tag: NodeDataTag) -> NodeHeaderWord {
        NodeHeaderWord(
            ((kind as u16 as u64) << Self::KIND_SHIFT)
                | ((data_tag as u8 as u64) << Self::TAG_SHIFT),
        )
    }

    /// Decoded without a range check: `Kind::from_i16`'s check measured +3% instructions on webpack here
    /// (notes/unsafe-audit.md, "Enum-from-integer decodes").
    #[inline]
    fn kind(self) -> Kind {
        let v = (self.0 >> Self::KIND_SHIFT) as u16 & ((1 << Self::KIND_BITS) - 1);
        debug_assert!(v <= Kind::Count as u16);
        // SAFETY: the bits were stored from a `Kind` (repr(i16), contiguous discriminants 0..=Count < 2^9).
        unsafe { std::mem::transmute::<i16, Kind>(v as i16) }
    }

    #[inline]
    fn with_rare_tail(self) -> NodeHeaderWord {
        NodeHeaderWord(self.0 | Self::RARE_BIT)
    }

    #[inline]
    fn has_rare_tail(self) -> bool {
        self.0 & Self::RARE_BIT != 0
    }

    #[inline]
    fn with_trailing_comma(self) -> NodeHeaderWord {
        NodeHeaderWord(self.0 | Self::TRAILING_COMMA_BIT)
    }

    #[inline]
    fn has_trailing_comma(self) -> bool {
        self.0 & Self::TRAILING_COMMA_BIT != 0
    }

    /// Decoded without a range check: a checked identity match measured +2% instructions on webpack here
    /// (notes/unsafe-audit.md, "Enum-from-integer decodes").
    #[inline]
    fn data_tag(self) -> NodeDataTag {
        let v = (self.0 >> Self::TAG_SHIFT) as u8;
        debug_assert!(v < NodeDataTag::COUNT);
        // SAFETY: the bits were stored from a `NodeDataTag` (repr(u8)).
        unsafe { std::mem::transmute::<u8, NodeDataTag>(v) }
    }

    #[inline]
    fn parent(self) -> Option<P<Node>> {
        // SAFETY: the low bits were stored by `with_parent` from a live `P<Node>` (arena nodes are never freed or moved).
        unsafe { P::unpack_opt(self.0) }
    }

    #[inline]
    fn with_parent(self, parent: Option<P<Node>>) -> NodeHeaderWord {
        NodeHeaderWord(self.0 & !Self::PARENT_MASK | P::pack_opt(parent))
    }
}

const _: () = assert!((Kind::Count as u64) < 1 << NodeHeaderWord::KIND_BITS);
const _: () = assert!(NodeHeaderWord::TAG_SHIFT + 9 == 63);

/// One arena allocation per node: the header, then the data struct. `repr(C)` puts the header at offset 0 and
/// the data at `offset_of!(NodeAlloc<T>, data)` (24 for every data struct: none is aligned to more than 8).
#[repr(C)]
#[expect(
    clippy::too_long_first_doc_paragraph,
    reason = "Public for the integration tests; the comment keeps its upstream wording."
)]
pub struct NodeAlloc<T> {
    pub node: Node,
    pub(crate) data: T,
}

/// A node data struct with fields, stored after the header of nodes tagged `TAG` (impls are generated).
pub(crate) trait NodePayload: Sized + 'static {
    const TAG: NodeDataTag;
}

/// `NodeAlloc` for a node with a rare tail: fields that are almost never set (on the private monorepo, e.g. 0.1%
/// of call expressions have a `?.` token and 0.5% type arguments) live in `rare`, allocated only when one of them is
/// set at construction, and read as `None` otherwise. Only fields that are never written after construction can be
/// rare (tools/gen-ast/gen-ast.ts `RARE_FIELDS`).
#[repr(C)]
struct NodeAllocRare<T, R> {
    node: Node,
    data: T,
    rare: R,
}

/// A node data struct with a rare tail (impls are generated).
pub(crate) trait NodeRareTail: NodePayload {
    type Rare: 'static;
    /// Compile-time check that the data struct sits at the same offset in both allocations.
    const SAME_OFFSET: () = assert!(
        std::mem::offset_of!(NodeAlloc<Self>, data)
            == std::mem::offset_of!(NodeAllocRare<Self, Self::Rare>, data)
    );
}

// Node accessors. Accessors that dispatch over the node data (name(), modifiers(), *_data(), as_*(),
// for_each_child(), ...) are generated in generated.rs.

impl Node {
    #[inline]
    pub fn kind(&self) -> Kind {
        self.header.get().kind()
    }

    /// Which data struct follows this node's header.
    #[inline]
    pub fn data_tag(&self) -> NodeDataTag {
        self.header.get().data_tag()
    }

    /// The data struct after this node's header. Callers check `data_tag() == T::TAG` first.
    #[inline]
    pub(crate) fn payload<T: NodePayload>(&self) -> &'static T {
        debug_assert_eq!(self.data_tag(), T::TAG);
        // SAFETY: a node tagged `T::TAG` was allocated by `new_node::<T>` as a `NodeAlloc<T>` whose header is
        // `self`, so its data struct lives at this offset from the header, for the rest of the process.
        unsafe {
            tsrs_core::ptr::arena_neighbor(self, std::mem::offset_of!(NodeAlloc<T>, data) as isize)
        }
    }

    /// The arena pointer for this node. Every `Node` is created by `NodeFactory`/`new_node` in the leak
    /// arena (Node has a crate-private field, so it cannot be constructed elsewhere), so `&self` is
    /// always a reference to a `'static` arena value.
    #[inline]
    pub(crate) fn as_p(&self) -> P<Node> {
        // SAFETY: see above; nodes are never freed or moved.
        unsafe { P::from_arena(&*std::ptr::from_ref::<Node>(self)) }
    }

    #[inline]
    pub fn pos(&self) -> TextPos {
        self.loc.get().pos()
    }
    #[inline]
    pub fn end(&self) -> TextPos {
        self.loc.get().end()
    }
    #[inline]
    pub fn loc(&self) -> TextRange {
        self.loc.get()
    }
    #[inline]
    pub fn set_loc(&self, loc: TextRange) {
        if self.data_tag() == NodeDataTag::Identifier && loc.end() != self.end() {
            crate::identifier::check_source_identifier_loc(self, loc);
        }
        self.loc.set(loc);
    }
    #[inline]
    pub fn flags(&self) -> NodeFlags {
        self.flags.get()
    }
    #[inline]
    pub fn set_flags(&self, flags: NodeFlags) {
        self.flags.set(flags);
    }
    #[inline]
    pub fn parent(&self) -> Option<P<Node>> {
        self.header.get().parent()
    }
    #[inline]
    pub fn set_parent(&self, parent: Option<P<Node>>) {
        self.header.set(self.header.get().with_parent(parent));
    }

    #[inline]
    pub fn has_trailing_comma_marker(&self) -> bool {
        self.header.get().has_trailing_comma()
    }

    #[inline]
    pub fn mark_trailing_comma(&self) {
        self.header.set(self.header.get().with_trailing_comma());
    }

    /// Go `IterChildren`: the children in `for_each_child` order.
    pub fn iter_children(&self) -> Vec<P<Node>> {
        let mut children = Vec::new();
        self.for_each_child(&mut |child| {
            children.push(child);
            false
        });
        children
    }

    pub fn parameter_list(&self) -> Option<P<NodeList>> {
        self.function_like_data().unwrap().parameters.get()
    }

    pub fn parameters(&self) -> &'static [P<Node>] {
        match self.parameter_list() {
            Some(list) => list.nodes(),
            None => &[],
        }
    }

    pub fn kind_string(&self) -> String {
        format!("{:?}", self.kind())
    }

    pub fn decorators(&self) -> Vec<P<Node>> {
        match self.modifiers() {
            None => Vec::new(),
            Some(modifiers) => {
                modifiers.nodes().iter().copied().filter(|&m| is_decorator(m)).collect()
            }
        }
    }

    /// Go `AsMutable()`: in Rust the mutators live on `Node` directly.
    #[inline]
    pub fn as_mutable(&self) -> &Node {
        self
    }

    pub fn set_modifiers(&self, modifiers: Option<P<ModifierList>>) {
        self.set_modifiers_data(modifiers);
    }

    pub fn symbol(&self) -> Option<P<Symbol>> {
        match self.declaration_data() {
            Some(data) => data.symbol.get(),
            None => None,
        }
    }

    pub fn local_symbol(&self) -> Option<P<Symbol>> {
        match self.exportable_data() {
            Some(data) => data.local_symbol.get(),
            None => None,
        }
    }

    pub fn locals(&self) -> Option<P<SymbolTable>> {
        match self.locals_container_data() {
            Some(data) => data.locals.get(),
            None => None,
        }
    }

    /// Go `node.FlowNodeData() != nil`.
    #[inline]
    pub fn has_flow_node_data(&self) -> bool {
        self.data_tag() == NodeDataTag::Identifier || self.flow_node_base().is_some()
    }

    /// Go `node.FlowNodeData().FlowNode` (nil when the node has no flow node data).
    #[inline]
    pub fn flow_node(&self) -> Option<P<FlowNode>> {
        if self.data_tag() == NodeDataTag::Identifier {
            return self.as_identifier().flow_node();
        }
        match self.flow_node_base() {
            Some(data) => data.flow_node.get(),
            None => None,
        }
    }

    /// Go `node.FlowNodeData().FlowNode = flow`; panics when the node has no flow node data (Go: nil dereference).
    #[inline]
    pub fn set_flow_node(&self, flow: Option<P<FlowNode>>) {
        if self.data_tag() == NodeDataTag::Identifier {
            return self.as_identifier().set_flow_node(flow);
        }
        self.flow_node_base().expect("node has no flow node data").flow_node.set(flow);
    }

    pub fn body(&self) -> Option<P<Node>> {
        match self.body_data() {
            Some(data) => data.body,
            None => None,
        }
    }

    /// Go `Text()`. Joined texts (`JsxNamespacedName`, `JSDoc` text) are allocated in the arena. Identifiers
    /// (most calls) inline; the other kinds out of line.
    #[inline]
    pub fn text(&self) -> &'static str {
        if self.kind() == Kind::Identifier {
            return self.as_identifier().text();
        }
        self.text_of_non_identifier()
    }

    #[inline(never)]
    fn text_of_non_identifier(&self) -> &'static str {
        match self.kind() {
            Kind::Identifier => self.as_identifier().text(),
            Kind::PrivateIdentifier => self.as_private_identifier().text(),
            Kind::StringLiteral => self.as_string_literal().text(),
            Kind::NumericLiteral => self.as_numeric_literal().text(),
            Kind::BigIntLiteral => self.as_big_int_literal().text(),
            Kind::MetaProperty => self.as_meta_property().name.text(),
            Kind::NoSubstitutionTemplateLiteral => {
                self.as_no_substitution_template_literal().text()
            }
            Kind::TemplateHead => self.as_template_head().text(),
            Kind::TemplateMiddle => self.as_template_middle().text(),
            Kind::TemplateTail => self.as_template_tail().text(),
            Kind::JsxNamespacedName => {
                let n = self.as_jsx_namespaced_name();
                alloc_str(&format!("{}:{}", n.namespace.text(), n.name.text()))
            }
            Kind::RegularExpressionLiteral => self.as_regular_expression_literal().text(),
            Kind::JSDocText => join_texts(self.as_jsdoc_text().text()),
            Kind::JSDocLink => join_texts(self.as_jsdoc_link().text()),
            Kind::JSDocLinkCode => join_texts(self.as_jsdoc_link_code().text()),
            Kind::JSDocLinkPlain => join_texts(self.as_jsdoc_link_plain().text()),
            _ => panic!("Unhandled case in Node.Text: {:?}", self.kind()),
        }
    }

    pub fn expression(&self) -> Option<P<Node>> {
        match self.data_tag() {
            NodeDataTag::PropertyAccessExpression => {
                Some(self.as_property_access_expression().expression)
            }
            NodeDataTag::ElementAccessExpression => {
                Some(self.as_element_access_expression().expression)
            }
            NodeDataTag::ParenthesizedExpression => {
                Some(self.as_parenthesized_expression().expression.get())
            }
            NodeDataTag::CallExpression => Some(self.as_call_expression().expression),
            NodeDataTag::NewExpression => Some(self.as_new_expression().expression),
            NodeDataTag::ExpressionWithTypeArguments => {
                Some(self.as_expression_with_type_arguments().expression)
            }
            NodeDataTag::ComputedPropertyName => Some(self.as_computed_property_name().expression),
            NodeDataTag::NonNullExpression => Some(self.as_non_null_expression().expression),
            NodeDataTag::TypeAssertion => Some(self.as_type_assertion().expression),
            NodeDataTag::AsExpression => Some(self.as_as_expression().expression),
            NodeDataTag::SatisfiesExpression => Some(self.as_satisfies_expression().expression),
            NodeDataTag::TypeOfExpression => Some(self.as_type_of_expression().expression),
            NodeDataTag::SpreadAssignment => Some(self.as_spread_assignment().expression),
            NodeDataTag::SpreadElement => Some(self.as_spread_element().expression),
            NodeDataTag::TemplateSpan => Some(self.as_template_span().expression),
            NodeDataTag::DeleteExpression => Some(self.as_delete_expression().expression),
            NodeDataTag::VoidExpression => Some(self.as_void_expression().expression),
            NodeDataTag::AwaitExpression => Some(self.as_await_expression().expression),
            NodeDataTag::YieldExpression => self.as_yield_expression().expression,
            NodeDataTag::PartiallyEmittedExpression => {
                Some(self.as_partially_emitted_expression().expression)
            }
            NodeDataTag::IfStatement => Some(self.as_if_statement().expression),
            NodeDataTag::DoStatement => Some(self.as_do_statement().expression),
            NodeDataTag::WhileStatement => Some(self.as_while_statement().expression),
            NodeDataTag::WithStatement => Some(self.as_with_statement().expression),
            NodeDataTag::ForInOrOfStatement
                if matches!(self.kind(), Kind::ForInStatement | Kind::ForOfStatement) =>
            {
                Some(self.as_for_in_or_of_statement().expression)
            }
            NodeDataTag::SwitchStatement => Some(self.as_switch_statement().expression),
            NodeDataTag::CaseOrDefaultClause if self.kind() == Kind::CaseClause => {
                self.as_case_or_default_clause().expression
            }
            NodeDataTag::ExpressionStatement => Some(self.as_expression_statement().expression),
            NodeDataTag::ReturnStatement => self.as_return_statement().expression.get(),
            NodeDataTag::ThrowStatement => Some(self.as_throw_statement().expression),
            NodeDataTag::ExternalModuleReference => {
                Some(self.as_external_module_reference().expression)
            }
            NodeDataTag::ExportAssignment => Some(self.as_export_assignment().expression.get()),
            NodeDataTag::Decorator => Some(self.as_decorator().expression),
            NodeDataTag::JsxExpression => self.as_jsx_expression().expression,
            NodeDataTag::JsxSpreadAttribute => Some(self.as_jsx_spread_attribute().expression),
            _ => panic!("Unhandled case in Node.Expression: {:?}", self.kind()),
        }
    }

    pub fn raw_text(&self) -> &'static str {
        match self.kind() {
            Kind::TemplateHead => self.as_template_head().raw_text(),
            Kind::TemplateMiddle => self.as_template_middle().raw_text(),
            Kind::TemplateTail => self.as_template_tail().raw_text(),
            _ => panic!("Unhandled case in Node.RawText: {:?}", self.kind()),
        }
    }

    /// Go `MutableNode.SetExpression`. Only the kinds the reparser mutates have a mutable `expression`
    /// field in this port; the other Go cases panic.
    pub fn set_expression(&self, expr: P<Node>) {
        match self.kind() {
            Kind::ParenthesizedExpression => {
                self.as_parenthesized_expression().expression.set(expr);
            }
            Kind::ReturnStatement => self.as_return_statement().expression.set(Some(expr)),
            Kind::ExportAssignment => self.as_export_assignment().expression.set(expr),
            _ => panic!("Unhandled case in mutableNode.SetExpression: {:?}", self.kind()),
        }
    }

    pub fn argument_list(&self) -> Option<P<NodeList>> {
        match self.kind() {
            Kind::CallExpression => Some(self.as_call_expression().arguments),
            Kind::NewExpression => self.as_new_expression().arguments,
            _ => panic!("Unhandled case in Node.Arguments: {:?}", self.kind()),
        }
    }

    pub fn arguments(&self) -> &'static [P<Node>] {
        match self.argument_list() {
            Some(list) => list.nodes(),
            None => &[],
        }
    }

    pub fn type_argument_list(&self) -> Option<P<NodeList>> {
        match self.kind() {
            Kind::CallExpression => self.as_call_expression().type_arguments(),
            Kind::NewExpression => self.as_new_expression().type_arguments(),
            Kind::TaggedTemplateExpression => self.as_tagged_template_expression().type_arguments,
            Kind::TypeReference => self.as_type_reference_node().type_arguments(),
            Kind::ExpressionWithTypeArguments => {
                self.as_expression_with_type_arguments().type_arguments.get()
            }
            Kind::ImportType => self.as_import_type_node().type_arguments(),
            Kind::TypeQuery => self.as_type_query_node().type_arguments(),
            Kind::JsxOpeningElement => self.as_jsx_opening_element().type_arguments,
            Kind::JsxSelfClosingElement => self.as_jsx_self_closing_element().type_arguments,
            _ => panic!("Unhandled case in Node.TypeArguments"),
        }
    }

    pub fn type_arguments(&self) -> &'static [P<Node>] {
        match self.type_argument_list() {
            Some(list) => list.nodes(),
            None => &[],
        }
    }

    pub fn type_parameter_list(&self) -> Option<P<NodeList>> {
        match self.kind() {
            Kind::ClassDeclaration => self.as_class_declaration().type_parameters(),
            Kind::ClassExpression => self.as_class_expression().type_parameters(),
            Kind::InterfaceDeclaration => self.as_interface_declaration().type_parameters,
            Kind::TypeAliasDeclaration | Kind::JSTypeAliasDeclaration => {
                self.as_type_alias_declaration().type_parameters.get()
            }
            Kind::JSDocTemplateTag => Some(self.as_jsdoc_template_tag().type_parameters),
            _ => {
                if let Some(func_like) = self.function_like_data() {
                    return func_like.type_parameters.get();
                }
                panic!("Unhandled case in Node.TypeParameterList")
            }
        }
    }

    pub fn type_parameters(&self) -> &'static [P<Node>] {
        match self.type_parameter_list() {
            Some(list) => list.nodes(),
            None => &[],
        }
    }

    pub fn member_list(&self) -> Option<P<NodeList>> {
        match self.kind() {
            Kind::ClassDeclaration => Some(self.as_class_declaration().members()),
            Kind::ClassExpression => Some(self.as_class_expression().members()),
            Kind::InterfaceDeclaration => Some(self.as_interface_declaration().members),
            Kind::EnumDeclaration => Some(self.as_enum_declaration().members),
            Kind::TypeLiteral => Some(self.as_type_literal_node().members),
            Kind::MappedType => self.as_mapped_type_node().members,
            _ => panic!("Unhandled case in Node.MemberList: {:?}", self.kind()),
        }
    }

    pub fn members(&self) -> &'static [P<Node>] {
        match self.member_list() {
            Some(list) => list.nodes(),
            None => &[],
        }
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "Node list accessors share an optional-list interface with nullable member, argument and parameter lists."
    )]
    pub fn statement_list(&self) -> Option<P<NodeList>> {
        match self.kind() {
            Kind::SourceFile => Some(self.as_source_file().statements),
            Kind::Block => Some(self.as_block().statements),
            Kind::ModuleBlock => Some(self.as_module_block().statements),
            Kind::CaseClause | Kind::DefaultClause => {
                Some(self.as_case_or_default_clause().statements)
            }
            _ => panic!("Unhandled case in Node.StatementList: {:?}", self.kind()),
        }
    }

    pub fn statements(&self) -> &'static [P<Node>] {
        match self.statement_list() {
            Some(list) => list.nodes(),
            None => &[],
        }
    }

    pub fn can_have_statements(&self) -> bool {
        matches!(
            self.kind(),
            Kind::SourceFile
                | Kind::Block
                | Kind::ModuleBlock
                | Kind::CaseClause
                | Kind::DefaultClause
        )
    }

    pub fn modifier_flags(&self) -> ModifierFlags {
        match self.modifiers() {
            Some(modifiers) => modifiers.modifier_flags,
            None => ModifierFlags::None,
        }
    }

    pub fn modifier_nodes(&self) -> &'static [P<Node>] {
        match self.modifiers() {
            Some(modifiers) => modifiers.nodes(),
            None => &[],
        }
    }

    /// Go `Type()`.
    pub fn type_node(&self) -> Option<P<Node>> {
        match self.data_tag() {
            NodeDataTag::VariableDeclaration => self.as_variable_declaration().type_.get(),
            NodeDataTag::ParameterDeclaration => self.as_parameter_declaration().type_.get(),
            NodeDataTag::PropertySignatureDeclaration => {
                self.as_property_signature_declaration().type_.get()
            }
            NodeDataTag::PropertyDeclaration => self.as_property_declaration().type_.get(),
            NodeDataTag::PropertyAssignment => self.as_property_assignment().type_.get(),
            NodeDataTag::ShorthandPropertyAssignment => {
                self.as_shorthand_property_assignment().type_.get()
            }
            NodeDataTag::TypePredicateNode => self.as_type_predicate_node().type_,
            NodeDataTag::ParenthesizedTypeNode => Some(self.as_parenthesized_type_node().type_),
            NodeDataTag::TypeOperatorNode => Some(self.as_type_operator_node().type_),
            NodeDataTag::MappedTypeNode => self.as_mapped_type_node().type_,
            NodeDataTag::TypeAssertion => Some(self.as_type_assertion().type_),
            NodeDataTag::AsExpression => Some(self.as_as_expression().type_),
            NodeDataTag::SatisfiesExpression => Some(self.as_satisfies_expression().type_),
            NodeDataTag::TypeAliasDeclaration => self.as_type_alias_declaration().type_.get(),
            NodeDataTag::NamedTupleMember => Some(self.as_named_tuple_member().type_),
            NodeDataTag::OptionalTypeNode => Some(self.as_optional_type_node().type_),
            NodeDataTag::RestTypeNode => Some(self.as_rest_type_node().type_),
            NodeDataTag::TemplateLiteralTypeSpan => {
                Some(self.as_template_literal_type_span().type_)
            }
            NodeDataTag::JSDocTypeExpression => Some(self.as_jsdoc_type_expression().type_),
            NodeDataTag::JSDocParameterOrPropertyTag => {
                self.as_jsdoc_parameter_or_property_tag().type_expression
            }
            NodeDataTag::JSDocNullableType => Some(self.as_jsdoc_nullable_type().type_),
            NodeDataTag::JSDocNonNullableType => Some(self.as_jsdoc_non_nullable_type().type_),
            NodeDataTag::JSDocOptionalType => Some(self.as_jsdoc_optional_type().type_),
            NodeDataTag::ExportAssignment => self.as_export_assignment().type_.get(),
            NodeDataTag::BinaryExpression => self.as_binary_expression().type_.get(),
            _ => {
                if let Some(func_like) = self.function_like_data() {
                    return func_like.type_.get();
                }
                None
            }
        }
    }

    /// Go `MutableNode.SetType`. Only the kinds the reparser mutates have a mutable type field in this
    /// port; the other Go cases panic.
    pub fn set_type(&self, t: Option<P<Node>>) {
        match self.kind() {
            Kind::VariableDeclaration => self.as_variable_declaration().type_.set(t),
            Kind::Parameter => self.as_parameter_declaration().type_.set(t),
            Kind::PropertySignature => self.as_property_signature_declaration().type_.set(t),
            Kind::PropertyDeclaration => self.as_property_declaration().type_.set(t),
            Kind::PropertyAssignment => self.as_property_assignment().type_.set(t),
            Kind::ShorthandPropertyAssignment => {
                self.as_shorthand_property_assignment().type_.set(t);
            }
            Kind::TypeAliasDeclaration | Kind::JSTypeAliasDeclaration => {
                self.as_type_alias_declaration().type_.set(t);
            }
            Kind::ExportAssignment => self.as_export_assignment().type_.set(t),
            Kind::BinaryExpression => self.as_binary_expression().type_.set(t),
            _ => {
                if let Some(func_like) = self.function_like_data() {
                    func_like.type_.set(t);
                } else {
                    panic!("Unhandled case in mutableNode.SetType: {:?}", self.kind());
                }
            }
        }
    }

    pub fn initializer(&self) -> Option<P<Node>> {
        match self.data_tag() {
            NodeDataTag::VariableDeclaration => self.as_variable_declaration().initializer.get(),
            NodeDataTag::ParameterDeclaration => self.as_parameter_declaration().initializer(),
            NodeDataTag::BindingElement => self.as_binding_element().initializer(),
            NodeDataTag::PropertyDeclaration => self.as_property_declaration().initializer.get(),
            NodeDataTag::PropertySignatureDeclaration => {
                self.as_property_signature_declaration().initializer()
            }
            NodeDataTag::PropertyAssignment => {
                Some(self.as_property_assignment().initializer.get())
            }
            NodeDataTag::EnumMember => self.as_enum_member().initializer,
            NodeDataTag::ForStatement => self.as_for_statement().initializer,
            NodeDataTag::ForInOrOfStatement
                if matches!(self.kind(), Kind::ForInStatement | Kind::ForOfStatement) =>
            {
                Some(self.as_for_in_or_of_statement().initializer)
            }
            NodeDataTag::JsxAttribute => self.as_jsx_attribute().initializer,
            _ => panic!("Unhandled case in Node.Initializer"),
        }
    }

    /// Go `MutableNode.SetInitializer`. Only the kinds the reparser mutates are supported.
    pub fn set_initializer(&self, initializer: P<Node>) {
        match self.kind() {
            Kind::VariableDeclaration => {
                self.as_variable_declaration().initializer.set(Some(initializer));
            }
            Kind::PropertyDeclaration => {
                self.as_property_declaration().initializer.set(Some(initializer));
            }
            Kind::PropertyAssignment => self.as_property_assignment().initializer.set(initializer),
            _ => panic!("Unhandled case in mutableNode.SetInitializer"),
        }
    }

    pub fn tag_name(&self) -> P<Node> {
        match self.kind() {
            Kind::JsxOpeningElement => self.as_jsx_opening_element().tag_name,
            Kind::JsxClosingElement => self.as_jsx_closing_element().tag_name,
            Kind::JsxSelfClosingElement => self.as_jsx_self_closing_element().tag_name,
            Kind::JSDocUnknownTag
            | Kind::JSDocAugmentsTag
            | Kind::JSDocImplementsTag
            | Kind::JSDocDeprecatedTag
            | Kind::JSDocPublicTag
            | Kind::JSDocPrivateTag
            | Kind::JSDocProtectedTag
            | Kind::JSDocReadonlyTag
            | Kind::JSDocOverrideTag
            | Kind::JSDocCallbackTag
            | Kind::JSDocOverloadTag
            | Kind::JSDocParameterTag
            | Kind::JSDocPropertyTag
            | Kind::JSDocReturnTag
            | Kind::JSDocThisTag
            | Kind::JSDocTypeTag
            | Kind::JSDocTemplateTag
            | Kind::JSDocTypedefTag
            | Kind::JSDocSeeTag
            | Kind::JSDocSatisfiesTag
            | Kind::JSDocThrowsTag
            | Kind::JSDocImportTag => self.jsdoc_tag_base().tag_name,
            _ => panic!("Unhandled case in Node.TagName: {:?}", self.kind()),
        }
    }

    fn jsdoc_tag_base(&self) -> &'static JSDocTagBase {
        match self.data() {
            NodeData::JSDocUnknownTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocAugmentsTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocImplementsTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocDeprecatedTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocPublicTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocPrivateTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocProtectedTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocReadonlyTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocOverrideTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocCallbackTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocOverloadTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocParameterOrPropertyTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocReturnTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocThisTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocTypeTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocTemplateTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocTypedefTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocSeeTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocSatisfiesTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocThrowsTag(d) => &d.jsdoc_tag_base,
            NodeData::JSDocImportTag(d) => &d.jsdoc_tag_base,
            _ => panic!("not a JSDoc tag: {:?}", self.kind()),
        }
    }

    pub fn property_name(&self) -> Option<P<Node>> {
        match self.kind() {
            Kind::ImportSpecifier => self.as_import_specifier().property_name(),
            Kind::ExportSpecifier => self.as_export_specifier().property_name,
            Kind::BindingElement => self.as_binding_element().property_name(),
            _ => None,
        }
    }

    pub fn property_name_or_name(&self) -> Option<P<Node>> {
        let mut name = self.property_name();
        if name.is_none() {
            name = self.name();
        }
        name
    }

    pub fn is_type_only(&self) -> bool {
        match self.kind() {
            Kind::ImportEqualsDeclaration => self.as_import_equals_declaration().is_type_only,
            Kind::ImportSpecifier => self.as_import_specifier().is_type_only,
            Kind::ImportClause => self.as_import_clause().phase_modifier.get() == Kind::TypeKeyword,
            Kind::ExportDeclaration => self.as_export_declaration().is_type_only,
            Kind::ExportSpecifier => self.as_export_specifier().is_type_only,
            _ => false,
        }
    }

    // If updating this function, also update `hasComment`.
    pub fn comment_list(&self) -> Option<P<NodeList>> {
        match self.kind() {
            Kind::JSDoc => Some(self.as_jsdoc().comment),
            Kind::JSDocUnknownTag
            | Kind::JSDocAugmentsTag
            | Kind::JSDocImplementsTag
            | Kind::JSDocDeprecatedTag
            | Kind::JSDocPublicTag
            | Kind::JSDocPrivateTag
            | Kind::JSDocProtectedTag
            | Kind::JSDocReadonlyTag
            | Kind::JSDocOverrideTag
            | Kind::JSDocCallbackTag
            | Kind::JSDocOverloadTag
            | Kind::JSDocParameterTag
            | Kind::JSDocPropertyTag
            | Kind::JSDocReturnTag
            | Kind::JSDocThisTag
            | Kind::JSDocTypeTag
            | Kind::JSDocTemplateTag
            | Kind::JSDocTypedefTag
            | Kind::JSDocSeeTag
            | Kind::JSDocSatisfiesTag
            | Kind::JSDocThrowsTag
            | Kind::JSDocImportTag => self.jsdoc_tag_base().comment,
            _ => panic!("Unhandled case in Node.CommentList: {:?}", self.kind()),
        }
    }

    pub fn comments(&self) -> &'static [P<Node>] {
        match self.comment_list() {
            Some(list) => list.nodes(),
            None => &[],
        }
    }

    pub fn label(&self) -> Option<P<Node>> {
        match self.kind() {
            Kind::LabeledStatement => Some(self.as_labeled_statement().label),
            Kind::BreakStatement => self.as_break_statement().label,
            Kind::ContinueStatement => self.as_continue_statement().label,
            _ => panic!("Unhandled case in Node.Label: {:?}", self.kind()),
        }
    }

    pub fn attributes(&self) -> Option<P<Node>> {
        match self.kind() {
            Kind::JsxOpeningElement => Some(self.as_jsx_opening_element().attributes),
            Kind::JsxSelfClosingElement => Some(self.as_jsx_self_closing_element().attributes),
            Kind::ModuleDeclaration => self.as_module_declaration().attributes,
            _ => panic!("Unhandled case in Node.Attributes: {:?}", self.kind()),
        }
    }

    pub fn children(&self) -> P<NodeList> {
        match self.kind() {
            Kind::JsxElement => self.as_jsx_element().children,
            Kind::JsxFragment => self.as_jsx_fragment().children,
            _ => panic!("Unhandled case in Node.Children: {:?}", self.kind()),
        }
    }

    pub fn module_specifier(&self) -> Option<P<Node>> {
        match self.kind() {
            Kind::ImportDeclaration | Kind::JSImportDeclaration => {
                Some(self.as_import_declaration().module_specifier)
            }
            Kind::ExportDeclaration => self.as_export_declaration().module_specifier,
            Kind::JSDocImportTag => Some(self.as_jsdoc_import_tag().module_specifier),
            _ => panic!("Unhandled case in Node.ModuleSpecifier: {:?}", self.kind()),
        }
    }

    pub fn import_clause(&self) -> Option<P<Node>> {
        match self.kind() {
            Kind::ImportDeclaration | Kind::JSImportDeclaration => {
                self.as_import_declaration().import_clause
            }
            Kind::JSDocImportTag => self.as_jsdoc_import_tag().import_clause,
            _ => panic!("Unhandled case in Node.ImportClause: {:?}", self.kind()),
        }
    }

    pub fn statement(&self) -> P<Node> {
        match self.kind() {
            Kind::DoStatement => self.as_do_statement().statement(),
            Kind::WhileStatement => self.as_while_statement().statement(),
            Kind::ForStatement => self.as_for_statement().statement(),
            Kind::ForInStatement | Kind::ForOfStatement => {
                self.as_for_in_or_of_statement().statement
            }
            Kind::WithStatement => self.as_with_statement().statement,
            Kind::LabeledStatement => self.as_labeled_statement().statement,
            _ => panic!("Unhandled case in Node.Statement: {:?}", self.kind()),
        }
    }

    pub fn property_list(&self) -> P<NodeList> {
        match self.kind() {
            Kind::ObjectLiteralExpression => self.as_object_literal_expression().properties,
            Kind::JsxAttributes => self.as_jsx_attributes().properties,
            _ => panic!("Unhandled case in Node.PropertyList: {:?}", self.kind()),
        }
    }

    pub fn properties(&self) -> &'static [P<Node>] {
        self.property_list().nodes()
    }

    pub fn element_list(&self) -> P<NodeList> {
        match self.kind() {
            Kind::NamedImports => self.as_named_imports().elements,
            Kind::NamedExports => self.as_named_exports().elements,
            Kind::ObjectBindingPattern | Kind::ArrayBindingPattern => {
                self.as_binding_pattern().elements
            }
            Kind::ArrayLiteralExpression => self.as_array_literal_expression().elements,
            Kind::TupleType => self.as_tuple_type_node().elements,
            _ => panic!("Unhandled case in Node.ElementList: {:?}", self.kind()),
        }
    }

    pub fn elements(&self) -> &'static [P<Node>] {
        self.element_list().nodes()
    }

    pub fn postfix_token(&self) -> Option<P<Node>> {
        match self.data_tag() {
            NodeDataTag::MethodDeclaration => self.as_method_declaration().postfix_token(),
            NodeDataTag::ShorthandPropertyAssignment => {
                self.as_shorthand_property_assignment().postfix_token()
            }
            NodeDataTag::MethodSignatureDeclaration => {
                self.as_method_signature_declaration().postfix_token()
            }
            NodeDataTag::PropertySignatureDeclaration => {
                self.as_property_signature_declaration().postfix_token()
            }
            NodeDataTag::PropertyAssignment => self.as_property_assignment().postfix_token(),
            NodeDataTag::PropertyDeclaration => self.as_property_declaration().postfix_token(),
            NodeDataTag::EnumMember => self.as_enum_member().postfix_token(),
            NodeDataTag::GetAccessorDeclaration => {
                self.as_get_accessor_declaration().postfix_token()
            }
            NodeDataTag::SetAccessorDeclaration => {
                self.as_set_accessor_declaration().postfix_token()
            }
            _ => None,
        }
    }

    pub fn question_token(&self) -> Option<P<Node>> {
        match self.data_tag() {
            NodeDataTag::ParameterDeclaration => {
                return self.as_parameter_declaration().question_token.get();
            }
            NodeDataTag::ConditionalExpression => {
                return Some(self.as_conditional_expression().question_token);
            }
            NodeDataTag::MappedTypeNode => return self.as_mapped_type_node().question_token,
            NodeDataTag::NamedTupleMember => return self.as_named_tuple_member().question_token,
            _ => {}
        }
        let postfix = self.postfix_token();
        if let Some(postfix) = postfix
            && postfix.kind() == Kind::QuestionToken
        {
            return Some(postfix);
        }
        None
    }

    pub fn question_dot_token(&self) -> Option<P<Node>> {
        match self.kind() {
            Kind::ElementAccessExpression => {
                self.as_element_access_expression().question_dot_token()
            }
            Kind::PropertyAccessExpression => {
                self.as_property_access_expression().question_dot_token()
            }
            Kind::CallExpression => self.as_call_expression().question_dot_token(),
            Kind::TaggedTemplateExpression => {
                self.as_tagged_template_expression().question_dot_token
            }
            _ => panic!("Unhandled case in Node.QuestionDotToken: {:?}", self.kind()),
        }
    }

    pub fn type_expression(&self) -> Option<P<Node>> {
        match self.kind() {
            Kind::JSDocParameterTag | Kind::JSDocPropertyTag => {
                self.as_jsdoc_parameter_or_property_tag().type_expression
            }
            Kind::JSDocReturnTag => self.as_jsdoc_return_tag().type_expression,
            Kind::JSDocTypeTag => Some(self.as_jsdoc_type_tag().type_expression),
            Kind::JSDocTypedefTag => self.as_jsdoc_typedef_tag().type_expression,
            Kind::JSDocCallbackTag => Some(self.as_jsdoc_callback_tag().type_expression),
            Kind::JSDocSatisfiesTag => Some(self.as_jsdoc_satisfies_tag().type_expression),
            Kind::JSDocThrowsTag => self.as_jsdoc_throws_tag().type_expression,
            _ => panic!("Unhandled case in Node.TypeExpression: {:?}", self.kind()),
        }
    }

    pub fn class_name(&self) -> P<Node> {
        match self.kind() {
            Kind::JSDocAugmentsTag => self.as_jsdoc_augments_tag().class_name,
            Kind::JSDocImplementsTag => self.as_jsdoc_implements_tag().class_name,
            _ => panic!("Unhandled case in Node.ClassName: {:?}", self.kind()),
        }
    }

    // Determines if `n` contains `descendant` by walking up the `Parent` pointers from `descendant`. This method panics if
    // `descendant` or one of its ancestors is not parented except when that node is a `SourceFile`.
    pub fn contains(&self, descendant: Option<P<Node>>) -> bool {
        let mut descendant = descendant;
        while let Some(d) = descendant {
            if std::ptr::eq(d.get(), self) {
                return true;
            }
            let parent = d.parent();
            assert!(parent.is_some() || is_source_file(d), "descendant is not parented");
            descendant = parent;
        }
        false
    }

    pub fn is_jsdoc(&self) -> bool {
        self.kind() == Kind::JSDoc
    }

    // if you provide nil for file, this code will walk to the root of the tree to find the file
    pub fn jsdoc(&self, file: Option<&'static SourceFile>) -> &'static [P<Node>] {
        if !self.flags.get().intersects(NodeFlags::HasJSDoc) {
            return &[];
        }
        let file = match file {
            Some(file) => file,
            None => match get_source_file_of_node(self.as_p()) {
                Some(file) => file.get(),
                None => return &[],
            },
        };
        if file.has_lazy_jsdoc.get() {
            return file.resolve_jsdoc(self.as_p());
        }
        file.parsed_jsdoc(self.as_p())
    }

    // EagerJSDoc returns JSDoc nodes that have already been parsed and cached,
    // without triggering lazy JSDoc parsing.
    pub fn eager_jsdoc(&self, file: Option<&'static SourceFile>) -> &'static [P<Node>] {
        if !self.flags.get().intersects(NodeFlags::HasJSDoc) {
            return &[];
        }
        let file = match file {
            Some(file) => file,
            None => match get_source_file_of_node(self.as_p()) {
                Some(file) => file.get(),
                None => return &[],
            },
        };
        if file.has_lazy_jsdoc.get() {
            return file.lazy_jsdoc_cache.read().unwrap().get(&self.as_p()).copied().unwrap_or(&[]);
        }
        file.parsed_jsdoc(self.as_p())
    }
}

fn join_texts(texts: &[&'static str]) -> &'static str {
    if texts.len() == 1 {
        return texts[0];
    }
    alloc_str(&texts.concat())
}

pub fn is_write_only_access(node: P<Node>) -> bool {
    access_kind(node) == AccessKind::Write
}

pub fn is_write_access(node: P<Node>) -> bool {
    access_kind(node) != AccessKind::Read
}

pub fn is_write_access_for_reference(node: P<Node>) -> bool {
    let decl = get_declaration_from_name(Some(node));
    (decl.is_some() && declaration_is_write_access(decl))
        || node.kind() == Kind::DefaultKeyword
        || is_write_access(node)
}

pub fn get_declaration_from_name(name: Option<P<Node>>) -> Option<P<Node>> {
    let name = name?;
    let parent = name.parent()?;
    let mut fallthrough = false;
    match name.kind() {
        Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral | Kind::NumericLiteral => {
            if is_computed_property_name(parent) {
                return parent.parent();
            }
            fallthrough = true;
        }
        Kind::Identifier => fallthrough = true,
        Kind::PrivateIdentifier if is_declaration(parent) && parent.name() == Some(name) => {
            return Some(parent);
        }
        _ => {}
    }
    if fallthrough {
        if is_declaration(parent) {
            if parent.name() == Some(name) {
                return Some(parent);
            }
            return None;
        }
        if is_qualified_name(parent) {
            let tag = parent.parent();
            if let Some(tag) = tag
                && is_jsdoc_parameter_tag(tag)
                && tag.name() == Some(parent)
            {
                return Some(tag);
            }
            return None;
        }
        let bin_exp = parent.parent();
        if let Some(bin_exp) = bin_exp
            && is_binary_expression(bin_exp)
            && get_assignment_declaration_kind(bin_exp) != JSDeclarationKind::None
        {
            // (binExp.left as BindableStaticNameExpression).symbol || binExp.symbol
            let left_has_symbol = bin_exp.as_binary_expression().left.symbol().is_some();
            if (left_has_symbol || bin_exp.symbol().is_some())
                && get_name_of_declaration(Some(bin_exp)) == Some(name)
            {
                return Some(bin_exp);
            }
        }
    }
    None
}

fn declaration_is_write_access(decl: Option<P<Node>>) -> bool {
    let Some(decl) = decl else {
        return false;
    };
    // Consider anything in an ambient declaration to be a write access since it may be coming from JS.
    if decl.flags.get().intersects(NodeFlags::Ambient) {
        return true;
    }

    match decl.kind() {
        Kind::BinaryExpression
        | Kind::BindingElement
        | Kind::ClassDeclaration
        | Kind::ClassExpression
        | Kind::DefaultKeyword
        | Kind::EnumDeclaration
        | Kind::EnumMember
        | Kind::ExportSpecifier
        | Kind::ImportClause // default import
        | Kind::ImportEqualsDeclaration
        | Kind::ImportSpecifier
        | Kind::InterfaceDeclaration
        | Kind::JSDocCallbackTag
        | Kind::JSDocTypedefTag
        | Kind::JsxAttribute
        | Kind::ModuleDeclaration
        | Kind::NamespaceExportDeclaration
        | Kind::NamespaceImport
        | Kind::NamespaceExport
        | Kind::Parameter
        | Kind::ShorthandPropertyAssignment
        | Kind::TypeAliasDeclaration
        | Kind::JSTypeAliasDeclaration
        | Kind::TypeParameter => true,

        // In `({ x: y } = 0);`, `x` is not a write access.
        Kind::PropertyAssignment => !is_array_literal_or_object_literal_destructuring_pattern(decl.parent()),

        // functions considered write if they provide a value (have a body)
        Kind::FunctionDeclaration
        | Kind::FunctionExpression
        | Kind::Constructor
        | Kind::MethodDeclaration
        | Kind::GetAccessor
        | Kind::SetAccessor => decl.body().is_some(),

        // variable/property write if initializer present or is in catch clause
        Kind::VariableDeclaration | Kind::PropertyDeclaration => {
            let has_init = match decl.kind() {
                Kind::VariableDeclaration => decl.as_variable_declaration().initializer.get().is_some(),
                _ => decl.as_property_declaration().initializer.get().is_some(),
            };
            has_init || decl.parent().is_some_and(is_catch_clause)
        }

        Kind::MethodSignature | Kind::PropertySignature | Kind::JSDocPropertyTag | Kind::JSDocParameterTag => false,

        // preserve TS behavior: crash on unexpected kinds
        _ => panic!("Unhandled case in declarationIsWriteAccess"),
    }
}

pub fn is_array_literal_or_object_literal_destructuring_pattern(node: Option<P<Node>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    if !(is_array_literal_expression(node) || is_object_literal_expression(node)) {
        return false;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    // [a,b,c] from:
    // [a, b, c] = someExpression;
    if is_binary_expression(parent)
        && parent.as_binary_expression().left == node
        && parent.as_binary_expression().operator_token.kind() == Kind::EqualsToken
    {
        return true;
    }
    // [a, b, c] from:
    // for([a, b, c] of expression)
    if is_for_of_statement(parent) && parent.initializer() == Some(node) {
        return true;
    }
    // {x, a: {a, b, c} } = someExpression
    if is_property_assignment(parent) {
        return is_array_literal_or_object_literal_destructuring_pattern(parent.parent());
    }
    // [a, b, c] of
    // [x, [a, b, c] ] = someExpression
    is_array_literal_or_object_literal_destructuring_pattern(Some(parent))
}

fn access_kind(node: P<Node>) -> AccessKind {
    let Some(parent) = node.parent() else {
        return AccessKind::Read;
    };
    match parent.kind() {
        Kind::PrefixUnaryExpression => {
            let operator = parent.as_prefix_unary_expression().operator;
            if operator == Kind::PlusPlusToken || operator == Kind::MinusMinusToken {
                return AccessKind::ReadWrite;
            }
            AccessKind::Read
        }
        Kind::PostfixUnaryExpression => {
            let operator = parent.as_postfix_unary_expression().operator;
            if operator == Kind::PlusPlusToken || operator == Kind::MinusMinusToken {
                return AccessKind::ReadWrite;
            }
            AccessKind::Read
        }
        Kind::BinaryExpression => {
            if parent.as_binary_expression().left == node {
                let operator = parent.as_binary_expression().operator_token;
                if is_assignment_operator(operator.kind()) {
                    if operator.kind() == Kind::EqualsToken {
                        return AccessKind::Write;
                    }
                    return AccessKind::ReadWrite;
                }
            }
            AccessKind::Read
        }
        Kind::PropertyAccessExpression => {
            if parent.as_property_access_expression().name != node {
                return AccessKind::Read;
            }
            access_kind(parent)
        }
        Kind::PropertyAssignment => {
            let parent_access = access_kind(parent.parent().unwrap());
            // In `({ x: varname }) = { x: 1 }`, the left `x` is a read, the right `x` is a write.
            if node == parent.as_property_assignment().name() {
                return reverse_access_kind(parent_access);
            }
            parent_access
        }
        Kind::ShorthandPropertyAssignment => {
            // Assume it's the local variable being accessed, since we don't check public properties for --noUnusedLocals.
            if Some(node)
                == parent.as_shorthand_property_assignment().object_assignment_initializer.get()
            {
                return AccessKind::Read;
            }
            access_kind(parent.parent().unwrap())
        }
        Kind::ParenthesizedExpression | Kind::ArrayLiteralExpression => access_kind(parent),
        Kind::ForInStatement | Kind::ForOfStatement => {
            if node == parent.as_for_in_or_of_statement().initializer {
                return AccessKind::Write;
            }
            AccessKind::Read
        }
        _ => AccessKind::Read,
    }
}

fn reverse_access_kind(a: AccessKind) -> AccessKind {
    match a {
        AccessKind::Read => AccessKind::Write,
        AccessKind::Write => AccessKind::Read,
        AccessKind::ReadWrite => AccessKind::ReadWrite,
    }
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum AccessKind {
    Read,      // Only reads from a variable
    Write,     // Only writes to a variable without ever reading it. E.g.: `x=1;`.
    ReadWrite, // Reads from and writes to a variable. E.g.: `f(x++);`, `x/=1`.
}

pub fn is_declaration_node(node: P<Node>) -> bool {
    node.declaration_data().is_some()
}

pub fn is_locals_container(node: P<Node>) -> bool {
    node.locals_container_data().is_some()
}

pub fn is_type_or_js_type_alias_declaration(node: P<Node>) -> bool {
    node.kind() == Kind::TypeAliasDeclaration || node.kind() == Kind::JSTypeAliasDeclaration
}

pub fn is_import_declaration_or_js_import_declaration(node: P<Node>) -> bool {
    node.kind() == Kind::ImportDeclaration || node.kind() == Kind::JSImportDeclaration
}

pub fn is_any_export_assignment(node: P<Node>) -> bool {
    node.kind() == Kind::ExportAssignment
}

impl NodeFactory {
    pub fn new_modifier(&self, kind: Kind) -> P<Node> {
        self.new_token(kind)
    }
}

impl ImportAttributes {
    /// Go `(*ImportAttributesNode).GetResolutionModeOverride`; `node` is the `ImportAttributes` node or None.
    pub(crate) fn get_resolution_mode_override(
        node: Option<P<Node>>,
        mut grammar_error_on_node: Option<
            &mut dyn FnMut(
                P<Node>,
                &'static diagnostics::Message,
                &[&dyn std::fmt::Display],
            ) -> bool,
        >,
    ) -> Option<ResolutionMode> {
        let node = node?;
        let attributes = node.as_import_attributes().attributes;
        let attribute = attributes
            .nodes()
            .iter()
            .copied()
            .find(|attribute| attribute.name().unwrap().text() == "resolution-mode")?;
        let elem = attribute.as_import_attribute();
        if !is_string_literal_like(elem.value) {
            return None;
        }
        let text = elem.value.text();
        if text != "import" && text != "require" {
            if let Some(f) = grammar_error_on_node.as_mut() {
                f(
                    elem.value,
                    &diagnostics::X_RESOLUTION_MODE_SHOULD_BE_EITHER_REQUIRE_OR_IMPORT,
                    &[],
                );
            }
            return None;
        }
        if text == "import" {
            Some(tsrs_core::RESOLUTION_MODE_ESM)
        } else {
            Some(tsrs_core::ModuleKind::COMMON_JS)
        }
    }
}

/// Free-function spelling of `ImportAttributes::get_resolution_mode_override` (Go's nil-safe method on
/// `*ImportAttributesNode`).
pub fn get_resolution_mode_override(
    node: Option<P<Node>>,
    grammar_error_on_node: Option<
        &mut dyn FnMut(P<Node>, &'static diagnostics::Message, &[&dyn std::fmt::Display]) -> bool,
    >,
) -> Option<ResolutionMode> {
    ImportAttributes::get_resolution_mode_override(node, grammar_error_on_node)
}

// PatternAmbientModule

pub struct PatternAmbientModule {
    pub pattern: tsrs_core::Pattern,
    pub symbol: P<Symbol>,
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum CommentDirectiveKind {
    #[default]
    Unknown,
    ExpectError,
    Ignore,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CommentDirective {
    pub loc: TextRange,
    pub kind: CommentDirectiveKind,
}

// SourceFile

#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct SourceFileMetaData {
    pub package_json_type: String,
    pub package_json_directory: String,
    pub implied_node_format: ResolutionMode,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CheckJsDirective {
    pub enabled: bool,
    pub range: CommentRange,
}

pub trait HasFileName {
    fn file_name(&self) -> &str;
    fn path(&self) -> &Path;
}

pub struct SourceFile {
    node: OwnedCell<Option<P<Node>>>, // back pointer to the SourceFile node, set by the factory
    pub(crate) declaration_base: DeclarationBase,
    pub(crate) locals_container_base: LocalsContainerBase,

    // Fields set by NewSourceFile
    parse_options: SourceFileParseOptions,
    text: &'static str,
    // Written once by the file loader (SetContentMapperInfo) before the file is published.
    content_mapper_info: OwnedCell<Option<P<ContentMapperSourceFileInfo>>>,
    pub statements: P<NodeList>,    // NodeList[*Statement]
    pub end_of_file_token: P<Node>, // TokenNode[*EndOfFileToken]

    // Fields set by parser
    diagnostics: OwnedCell<&'static [P<Diagnostic>]>,
    js_diagnostics: OwnedCell<&'static [P<Diagnostic>]>,
    jsdoc_diagnostics: OwnedCell<&'static [P<Diagnostic>]>,
    pub language_variant: OwnedCell<LanguageVariant>,
    pub script_kind: OwnedCell<ScriptKind>,
    pub is_declaration_file: OwnedCell<bool>,
    pub uses_uri_style_node_core_modules: OwnedCell<Tristate>,
    pub identifier_count: OwnedCell<usize>,
    pub text_index: OwnedCell<u32>, // `register_source_text` index of `text` (compact identifiers, identifier.rs)
    pub imports: OwnedCell<&'static [P<Node>]>, // []LiteralLikeNode
    pub module_augmentations: OwnedCell<&'static [P<Node>]>, // []ModuleName
    pub ambient_module_names: OwnedCell<&'static [&'static str]>,
    pub comment_directives: OwnedCell<&'static [CommentDirective]>,
    // Go `jsdocCache`, in one of two forms per file. Without lazy JSDoc (JS files): the JSDoc the parser attached,
    // set once by `set_jsdoc_cache` and read without a lock. With lazy JSDoc: the parser's entries plus the ones
    // `resolve_jsdoc` parses on demand from any checker thread, under the lock (Go `jsdocMu`).
    jsdoc_cache: OnceLock<FxHashMap<P<Node>, &'static [P<Node>]>>,
    lazy_jsdoc_cache: RwLock<FxHashMap<P<Node>, &'static [P<Node>]>>,
    has_lazy_jsdoc: OwnedCell<bool>,
    identifiers: OnceLock<Set<&'static str>>,
    // ast.go:2517 nameTableOnce/nameTable (Go map, random order; insertion order here)
    name_table: OnceLock<tsrs_core::collections::OrderedMap<&'static str, NameTableEntry>>,
    pub reparsed_clones: OwnedCell<&'static [P<Node>]>,
    // tsrs-only: the member lists of this declaration file that are parsed and bound on first use (`lazylist`).
    pub lazy_lists: OwnedCell<&'static [P<crate::lazylist::LazyNodeList>]>,
    pub pragmas: OwnedCell<&'static [Pragma]>,
    pub referenced_files: OwnedCell<&'static [P<FileReference>]>,
    pub type_reference_directives: OwnedCell<&'static [P<FileReference>]>,
    pub lib_reference_directives: OwnedCell<&'static [P<FileReference>]>,
    pub check_js_directive: OwnedCell<Option<P<CheckJsDirective>>>,
    pub node_count: OwnedCell<usize>,
    pub text_count: OwnedCell<usize>,
    // Go `Hash xxh3.Uint128`: the content hash the project system's parse cache keys files by (0 = unset).
    pub hash: OwnedCell<u128>,
    pub common_js_module_indicator: OwnedCell<Option<P<Node>>>,
    // If this is the SourceFile itself, then this module was "forced"
    // to be an external module (previously "true").
    pub external_module_indicator: OwnedCell<Option<P<Node>>>,

    // Fields set by binder
    is_bound: AtomicBool,
    bind_once: Once,
    bind_diagnostics: OwnedCell<&'static [P<Diagnostic>]>,
    pub symbol_count: OwnedCell<usize>,
    pub pattern_ambient_modules: OwnedCell<&'static [P<PatternAmbientModule>]>,
    pub global_exports: OwnedCell<Option<P<SymbolTable>>>,

    // tsrs-only: set before the type-check pass when the CLI frees this file's tree once it is checked
    // (`tsrs_compiler` fileregions.rs); see `is_check_leaf`.
    check_leaf: AtomicBool,

    // Fields set by ECMALineMap
    ecma_line_map: OnceLock<&'static [u32]>,

    // Fields for UTF-8 to UTF-16 position mapping
    position_map: OnceLock<P<PositionMap>>,

    // Language service token cache (Go `tokenCacheMu`, `tokenCache`), see get_or_create_token
    token_cache: std::sync::Mutex<FxHashMap<TokenCacheKey, P<Node>>>,

    // Go `declarationMapMu`, `declarationMap` (workspace symbols), see get_declaration_map
    declaration_map: OnceLock<FxHashMap<String, Vec<P<Node>>>>,

    // Go `sourceFileSymbolIndexKey` data (api/session.go getSourceFileSymbolIndex), see get_or_compute_api_symbol_index
    api_symbol_index: OnceLock<FxHashMap<u64, P<Symbol>>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NameTableEntry {
    Unique(u32),
    Multiple,
}

impl NodeFactory {
    pub fn new_source_file(
        &self,
        opts: SourceFileParseOptions,
        text: &'static str,
        statements: P<NodeList>,
        end_of_file_token: P<Node>,
    ) -> P<Node> {
        if tsrs_core::tspath::get_encoded_root_length(&opts.file_name)
            == tsrs_core::tspath::EncodedRootLength::Relative
            || opts.file_name != tsrs_core::tspath::normalize_path(&opts.file_name)
        {
            panic!("fileName should be normalized and absolute: {:?}", opts.file_name);
        }
        let node = self.new_node(
            Kind::SourceFile,
            SourceFile {
                node: OwnedCell::new(None),
                declaration_base: DeclarationBase { symbol: OwnedCell::new(None) },
                locals_container_base: LocalsContainerBase {
                    locals: OwnedCell::new(None),
                    next_container: OwnedCell::new(None),
                },
                parse_options: opts,
                text,
                content_mapper_info: OwnedCell::new(None),
                statements,
                end_of_file_token,
                diagnostics: OwnedCell::new(&[]),
                js_diagnostics: OwnedCell::new(&[]),
                jsdoc_diagnostics: OwnedCell::new(&[]),
                language_variant: OwnedCell::new(LanguageVariant::default()),
                script_kind: OwnedCell::new(ScriptKind::default()),
                is_declaration_file: OwnedCell::new(false),
                uses_uri_style_node_core_modules: OwnedCell::new(Tristate::Unknown),
                identifier_count: OwnedCell::new(0),
                text_index: OwnedCell::new(crate::identifier::NO_SOURCE_TEXT),
                imports: OwnedCell::new(&[]),
                module_augmentations: OwnedCell::new(&[]),
                ambient_module_names: OwnedCell::new(&[]),
                comment_directives: OwnedCell::new(&[]),
                jsdoc_cache: OnceLock::new(),
                lazy_jsdoc_cache: RwLock::new(FxHashMap::default()),
                has_lazy_jsdoc: OwnedCell::new(false),
                identifiers: OnceLock::new(),
                name_table: OnceLock::new(),
                reparsed_clones: OwnedCell::new(&[]),
                lazy_lists: OwnedCell::new(&[]),
                pragmas: OwnedCell::new(&[]),
                referenced_files: OwnedCell::new(&[]),
                type_reference_directives: OwnedCell::new(&[]),
                lib_reference_directives: OwnedCell::new(&[]),
                check_js_directive: OwnedCell::new(None),
                node_count: OwnedCell::new(0),
                text_count: OwnedCell::new(0),
                hash: OwnedCell::new(0),
                common_js_module_indicator: OwnedCell::new(None),
                external_module_indicator: OwnedCell::new(None),
                is_bound: AtomicBool::new(false),
                bind_once: Once::new(),
                bind_diagnostics: OwnedCell::new(&[]),
                symbol_count: OwnedCell::new(0),
                pattern_ambient_modules: OwnedCell::new(&[]),
                global_exports: OwnedCell::new(None),
                check_leaf: AtomicBool::new(false),
                ecma_line_map: OnceLock::new(),
                position_map: OnceLock::new(),
                token_cache: std::sync::Mutex::new(FxHashMap::default()),
                declaration_map: OnceLock::new(),
                api_symbol_index: OnceLock::new(),
            },
        );
        node.as_source_file().node.set(Some(node));
        node
    }
}

impl SourceFile {
    /// The `SourceFile` node (Go `file.AsNode()`).
    #[inline]
    pub fn as_node(&self) -> P<Node> {
        self.node.get().unwrap()
    }

    /// `P<SourceFile>` for this file.
    #[inline]
    pub fn as_p(&self) -> P<SourceFile> {
        self.as_node().as_source_file_p()
    }

    pub fn parse_options(&self) -> &SourceFileParseOptions {
        &self.parse_options
    }

    pub fn text(&self) -> &'static str {
        self.text
    }

    // OriginalText returns the untransformed source text for content-mapped files, or Text() otherwise.
    // ast.go:2548
    pub fn original_text(&self) -> &'static str {
        if !self.content_mapper().is_empty() {
            return self.content_mapper_info.get().unwrap().original_text;
        }
        self.text
    }

    // OriginalFileName returns the canonical filename associated with a supplemental source file, or FileName() otherwise.
    // ast.go:2556
    pub fn original_file_name(&self) -> &str {
        if let Some(canonical) = self.canonical_source_file() {
            return canonical.get().file_name();
        }
        self.file_name()
    }

    // SpanMap returns the span map that maps positions in this file's transformed Text() back to its
    // original, untransformed content, or nil if the file is not content-mapped (or is a failure stub).
    // The returned map is nil-safe: a nil map maps positions identically.
    // ast.go:2566 (the nil-safe calls are the `tsrs_spanmap` free functions taking `Option<&SpanMap>`.)
    pub fn span_map(&self) -> Option<P<SpanMap>> {
        let info = self.content_mapper_info.get()?;
        info.span_map
    }

    // IsContentMapped reports whether this file was produced by a content mapper.
    // ast.go:2574
    pub fn is_content_mapped(&self) -> bool {
        self.content_mapper_info.get().is_some()
    }

    // ContentMapper returns the identity of the content mapper that produced this file, or "" if the file
    // was not produced by a content mapper (or the mapper did not identify itself).
    // ast.go:2580
    pub fn content_mapper(&self) -> &'static str {
        match self.content_mapper_info.get() {
            None => "",
            Some(info) => info.content_mapper,
        }
    }

    // IsContentMapperFailureStub reports whether this file is the empty placeholder produced when a content
    // mapper's transform failed.
    // ast.go:2589
    pub fn is_content_mapper_failure_stub(&self) -> bool {
        !self.content_mapper().is_empty() && self.span_map().is_none()
    }

    // ast.go:2593
    pub fn content_mapper_transform_identity(&self) -> &'static str {
        match self.content_mapper_info.get() {
            None => "",
            Some(info) => info.transform_identity,
        }
    }

    // ast.go:2600
    pub fn virtual_file_name(&self) -> &'static str {
        match self.content_mapper_info.get() {
            None => "",
            Some(info) => info.virtual_file_name,
        }
    }
}

// ast.go:2607
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum MappedDiagnosticDirectivePolicy {
    #[default]
    Ignore,
    Expect,
}

// ast.go:2614
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct MappedDiagnosticDirective {
    pub original_range: TextRange,
    pub virtual_range: TextRange,
    pub policy: MappedDiagnosticDirectivePolicy,
    pub unused_code: i32,
    pub unused_message_text: &'static str,
    pub source: &'static str,
}

// ast.go:2623 (strings and slices are arena data, like the other fields of a published SourceFile.)
#[derive(Clone, Debug, Default)]
pub struct ContentMapperSourceFileInfo {
    pub content_mapper: &'static str,
    pub transform_identity: &'static str,
    pub parse_options: SourceFileParseOptions,
    pub virtual_file_name: &'static str,
    pub original_text: &'static str,
    pub span_map: Option<P<SpanMap>>,
    pub diagnostic_directives: &'static [MappedDiagnosticDirective],
    pub supplemental_source_files: &'static [P<SourceFile>],
    pub canonical_source_file: Option<P<SourceFile>>,
}

impl SourceFile {
    // SetContentMapperInfo initializes all content-mapper metadata before the source file is published.
    // ast.go:2644
    pub fn set_content_mapper_info(&self, info: ContentMapperSourceFileInfo) {
        assert!(
            self.content_mapper_info.get().is_none(),
            "content mapper source file info already set"
        );
        self.content_mapper_info.set(Some(P::new(info)));
    }

    // ast.go:2651
    pub fn diagnostic_directives(&self) -> &'static [MappedDiagnosticDirective] {
        match self.content_mapper_info.get() {
            None => &[],
            Some(info) => info.diagnostic_directives,
        }
    }

    // SupplementalSourceFiles returns the additional outputs produced from this canonical source file.
    // ast.go:2659
    pub fn supplemental_source_files(&self) -> &'static [P<SourceFile>] {
        match self.content_mapper_info.get() {
            None => &[],
            Some(info) => info.supplemental_source_files,
        }
    }

    // CanonicalSourceFile returns the canonical output associated with this supplemental source file.
    // ast.go:2667
    pub fn canonical_source_file(&self) -> Option<P<SourceFile>> {
        self.content_mapper_info.get()?.canonical_source_file
    }

    // IsContentMapperSupplemental reports whether this is an unnamed supplemental mapper output.
    // ast.go:2675
    pub fn is_content_mapper_supplemental(&self) -> bool {
        self.canonical_source_file().is_some()
    }

    // GetNameTable returns a map of all names in the file to their positions.
    // If the name appears more than once, the value is `Multiple`.
    // ast.go:2857
    pub fn get_name_table(
        &self,
    ) -> &tsrs_core::collections::OrderedMap<&'static str, NameTableEntry> {
        if let Some(t) = self.name_table.get() {
            return t;
        }
        let _region = self.owner_region();
        self.name_table.get_or_init(|| {
            fn walk(
                node: P<Node>,
                file: &'static SourceFile,
                name_table: &mut tsrs_core::collections::OrderedMap<&'static str, NameTableEntry>,
            ) -> bool {
                if is_identifier(node) && !is_tag_name(node) && !node.text().is_empty()
                    || is_string_or_numeric_literal_like(node) && literal_is_name(node)
                    || is_private_identifier(node)
                {
                    let text = node.text();
                    // Go stores Pos() unchanged, so a reparsed name without a position (the `this` parameter
                    // a JSDoc `@this` tag adds) stores -1, the same value as "appears more than once".
                    name_table
                        .entry(text)
                        .and_modify(|entry| *entry = NameTableEntry::Multiple)
                        .or_insert_with(|| match node.pos().source_offset() {
                            Some(pos) => NameTableEntry::Unique(pos),
                            None => NameTableEntry::Multiple,
                        });
                }

                node.for_each_child(&mut |c| walk(c, file, name_table));
                let jsdoc_nodes = node.jsdoc(Some(file));
                for &jsdoc in jsdoc_nodes {
                    jsdoc.for_each_child(&mut |c| walk(c, file, name_table));
                }
                false
            }

            let mut name_table: tsrs_core::collections::OrderedMap<&'static str, NameTableEntry> =
                tsrs_core::collections::OrderedMap::default();
            let file: &'static SourceFile = self.as_node().as_source_file();
            self.as_node().for_each_child(&mut |c| walk(c, file, &mut name_table));
            name_table
        })
    }

    pub fn has_identifier(&self, name: &str) -> bool {
        if let Some(ids) = self.identifiers.get() {
            return ids.keys().contains(name);
        }
        let _region = self.owner_region();
        self.identifiers
            .get_or_init(|| collect_identifiers_for_source_file(self))
            .keys()
            .contains(name)
    }

    // Lazily filled shared data of the file lives in the file's own region in the language server (docs/LSP.md
    // "Memory plan for a long-lived server"), not in the region of whichever checker fills it. Take the scope before
    // any lock of the cache it fills (lock order: region, then cache). No-op without regions (CLI).
    #[inline]
    fn owner_region(&self) -> Option<tsrs_core::arena::RegionScope> {
        tsrs_core::arena::enter_owner(std::ptr::from_ref::<SourceFile>(self) as usize)
    }

    pub fn file_name(&self) -> &str {
        &self.parse_options.file_name
    }

    pub fn path(&self) -> &Path {
        &self.parse_options.path
    }

    pub fn imports(&self) -> &'static [P<Node>] {
        self.imports.get()
    }

    pub fn diagnostics(&self) -> &'static [P<Diagnostic>] {
        self.diagnostics.get()
    }

    pub fn set_diagnostics(&self, diags: &[P<Diagnostic>]) {
        self.diagnostics.set(alloc_slice(diags));
    }

    pub fn js_diagnostics(&self) -> &'static [P<Diagnostic>] {
        self.js_diagnostics.get()
    }

    pub fn set_js_diagnostics(&self, diags: &[P<Diagnostic>]) {
        self.js_diagnostics.set(alloc_slice(diags));
    }

    pub fn jsdoc_diagnostics(&self) -> &'static [P<Diagnostic>] {
        self.jsdoc_diagnostics.get()
    }

    pub fn set_jsdoc_diagnostics(&self, diags: &[P<Diagnostic>]) {
        self.jsdoc_diagnostics.set(alloc_slice(diags));
    }

    /// Stores the `JSDoc` the parser attached while parsing. The parser calls it once per file, after
    /// `set_has_lazy_jsdoc`, which decides which map holds the entries.
    pub fn set_jsdoc_cache(&self, cache: FxHashMap<P<Node>, &'static [P<Node>]>) {
        if self.has_lazy_jsdoc.get() {
            let mut lazy = self.lazy_jsdoc_cache.write().unwrap();
            assert!(lazy.is_empty(), "SourceFile JSDoc cache set twice");
            *lazy = cache;
        } else {
            let set = self.jsdoc_cache.set(cache);
            assert!(set.is_ok(), "SourceFile JSDoc cache set twice");
        }
    }

    /// The `JSDoc` the parser attached to `n` in a file without lazy `JSDoc`. No lock: the map is set once,
    /// before the file is shared.
    #[inline]
    fn parsed_jsdoc(&self, n: P<Node>) -> &'static [P<Node>] {
        self.jsdoc_cache.get().and_then(|cache| cache.get(&n)).copied().unwrap_or(&[])
    }

    pub fn set_has_lazy_jsdoc(&self, lazy: bool) {
        self.has_lazy_jsdoc.set(lazy);
    }

    fn resolve_jsdoc(&'static self, n: P<Node>) -> &'static [P<Node>] {
        let Some(parse) = PARSE_JSDOC_FOR_NODE.get() else {
            panic!(
                "resolveJSDoc called but parseJSDocForNode is not registered; ensure the parser package is imported"
            );
        };
        // Fast path: check cache under read lock
        {
            let cache = self.lazy_jsdoc_cache.read().unwrap();
            if let Some(&jsdocs) = cache.get(&n) {
                return jsdocs;
            }
        }
        // Slow path: parse and cache under write lock
        let _region = self.owner_region();
        let mut cache = self.lazy_jsdoc_cache.write().unwrap();
        // Double-check after acquiring write lock
        if let Some(&jsdocs) = cache.get(&n) {
            return jsdocs;
        }
        let jsdocs = alloc_vec(parse(self, n));
        cache.insert(n, jsdocs);
        jsdocs
    }

    pub fn bind_diagnostics(&self) -> &'static [P<Diagnostic>] {
        self.bind_diagnostics.get()
    }

    pub fn set_bind_diagnostics(&self, diags: &[P<Diagnostic>]) {
        self.bind_diagnostics.set(alloc_slice(diags));
    }

    pub(crate) fn for_each_child<V: FnMut(P<Node>) -> bool + ?Sized>(&self, v: &mut V) -> bool {
        visit_node_list(v, Some(self.statements)) || visit(v, Some(self.end_of_file_token))
    }

    pub(crate) fn visit_each_child(&self, node: P<Node>, v: &mut NodeVisitor) -> P<Node> {
        let statements = v.visit_top_level_statements_hooked(Some(self.statements)).unwrap();
        let end_of_file_token = v.visit_token_hooked(Some(self.end_of_file_token)).unwrap();
        v.factory.update_source_file(node, statements, end_of_file_token)
    }

    pub fn is_js(&self) -> bool {
        is_source_file_js(self.as_p())
    }

    fn copy_from(&self, other: &SourceFile) {
        // Do not copy fields set by NewSourceFile (Text, FileName, Path, or Statements)
        if let Some(info) = other.content_mapper_info.get() {
            self.set_content_mapper_info(ContentMapperSourceFileInfo::clone(&info));
        }
        self.language_variant.set(other.language_variant.get());
        self.script_kind.set(other.script_kind.get());
        self.is_declaration_file.set(other.is_declaration_file.get());
        self.uses_uri_style_node_core_modules.set(other.uses_uri_style_node_core_modules.get());
        self.imports.set(other.imports.get());
        self.module_augmentations.set(other.module_augmentations.get());
        self.ambient_module_names.set(other.ambient_module_names.get());
        self.comment_directives.set(other.comment_directives.get());
        self.pragmas.set(other.pragmas.get());
        self.referenced_files.set(other.referenced_files.get());
        self.type_reference_directives.set(other.type_reference_directives.get());
        self.lib_reference_directives.set(other.lib_reference_directives.get());
        self.common_js_module_indicator.set(other.common_js_module_indicator.get());
        self.external_module_indicator.set(other.external_module_indicator.get());
        let node = self.as_node();
        node.flags.set(node.flags.get() | other.as_node().flags.get());
    }

    pub(crate) fn clone_node(&self, node: P<Node>, f: &NodeFactory) -> P<Node> {
        let updated = f.new_source_file(
            self.parse_options.clone(),
            self.text,
            self.statements,
            self.end_of_file_token,
        );
        let new_file = updated.as_source_file();
        new_file.copy_from(self);
        clone_node(updated, node, &f.hooks)
    }

    /// `ecma_line_map().len()` without computing and keeping the map when it does not exist yet (the
    /// `--extendedDiagnostics` line count would otherwise build every file's map at the end of the run).
    pub fn ecma_line_count(&self) -> usize {
        if let Some(m) = self.ecma_line_map.get() {
            return m.len();
        }
        tsrs_core::count_ecma_line_starts(self.text)
    }

    pub fn ecma_line_map(&self) -> &'static [u32] {
        if let Some(&m) = self.ecma_line_map.get() {
            return m;
        }
        let _region = self.owner_region();
        self.ecma_line_map.get_or_init(|| alloc_vec(compute_ecma_line_starts(self.text)))
    }

    pub fn is_bound(&self) -> bool {
        self.is_bound.load(Ordering::Acquire)
    }

    /// tsrs-only: no other file refers to this file and it declares nothing another file can reach, and its tree and
    /// binder output are freed right after the checker that checks it is done with it (CLI `--noEmit`, `tsrs_compiler`
    /// fileregions.rs). Only that checker, while checking it, may read its tree; whole-program scans skip it otherwise.
    pub fn is_check_leaf(&self) -> bool {
        // Relaxed: written before the checker threads of the pass are spawned, which orders it before their reads.
        self.check_leaf.load(Ordering::Relaxed)
    }

    pub fn set_check_leaf(&self, leaf: bool) {
        // Relaxed: see `is_check_leaf`.
        self.check_leaf.store(leaf, Ordering::Relaxed);
    }

    // GetPositionMap returns the PositionMap for this source file, computing it lazily.
    pub fn get_position_map(&self) -> P<PositionMap> {
        if let Some(&m) = self.position_map.get() {
            return m;
        }
        let _region = self.owner_region();
        *self.position_map.get_or_init(|| P::new(compute_position_map(self.text)))
    }

    pub fn bind_once(&self, bind: impl FnOnce()) {
        self.bind_once.call_once(|| {
            bind();
            self.is_bound.store(true, Ordering::Release);
        });
    }

    // Getters mirroring Go's exported fields.

    pub fn language_variant(&self) -> LanguageVariant {
        self.language_variant.get()
    }
    pub fn script_kind(&self) -> ScriptKind {
        self.script_kind.get()
    }
    pub fn is_declaration_file(&self) -> bool {
        self.is_declaration_file.get()
    }
    pub fn uses_uri_style_node_core_modules(&self) -> Tristate {
        self.uses_uri_style_node_core_modules.get()
    }

    pub fn module_augmentations(&self) -> &'static [P<Node>] {
        self.module_augmentations.get()
    }
    pub fn ambient_module_names(&self) -> &'static [&'static str] {
        self.ambient_module_names.get()
    }

    pub(crate) fn reparsed_clones(&self) -> &'static [P<Node>] {
        self.reparsed_clones.get()
    }
    pub fn pragmas(&self) -> &'static [Pragma] {
        self.pragmas.get()
    }
    pub fn referenced_files(&self) -> &'static [P<FileReference>] {
        self.referenced_files.get()
    }
    pub fn type_reference_directives(&self) -> &'static [P<FileReference>] {
        self.type_reference_directives.get()
    }
    pub fn lib_reference_directives(&self) -> &'static [P<FileReference>] {
        self.lib_reference_directives.get()
    }
    pub fn check_js_directive(&self) -> Option<P<CheckJsDirective>> {
        self.check_js_directive.get()
    }

    pub fn common_js_module_indicator(&self) -> Option<P<Node>> {
        self.common_js_module_indicator.get()
    }
    pub fn external_module_indicator(&self) -> Option<P<Node>> {
        self.external_module_indicator.get()
    }
    pub fn symbol_count(&self) -> usize {
        self.symbol_count.get()
    }
    pub fn pattern_ambient_modules(&self) -> &'static [P<PatternAmbientModule>] {
        self.pattern_ambient_modules.get()
    }
    pub fn global_exports(&self) -> Option<P<SymbolTable>> {
        self.global_exports.get()
    }
    pub fn symbol(&self) -> Option<P<Symbol>> {
        self.declaration_base.symbol.get()
    }
    pub fn locals(&self) -> Option<P<SymbolTable>> {
        self.locals_container_base.locals.get()
    }
    pub fn statements(&self) -> P<NodeList> {
        self.statements
    }
}

impl HasFileName for SourceFile {
    fn file_name(&self) -> &str {
        SourceFile::file_name(self)
    }
    fn path(&self) -> &Path {
        SourceFile::path(self)
    }
}

impl Node {
    /// `P<FlowReduceLabelData>` for a reduce-label node.
    #[inline]
    pub fn as_flow_reduce_label_data_p(&self) -> P<FlowReduceLabelData> {
        // SAFETY: as in `as_source_file_p`.
        unsafe { P::from_arena(self.as_flow_reduce_label_data()) }
    }

    /// `P<SourceFile>` for a `SourceFile` node (Go code that holds `*ast.SourceFile`).
    #[inline]
    pub fn as_source_file_p(&self) -> P<SourceFile> {
        // SAFETY: the data struct of an arena node (8-aligned: it follows the 24-byte header).
        unsafe { P::from_arena(self.as_source_file()) }
    }
}

fn collect_identifiers_for_source_file(source_file: &SourceFile) -> Set<&'static str> {
    fn collect(node: P<Node>, identifiers: &mut Set<&'static str>) -> bool {
        match node.kind() {
            Kind::Identifier
            | Kind::PrivateIdentifier
            | Kind::StringLiteral
            | Kind::NumericLiteral
            | Kind::BigIntLiteral
            | Kind::NoSubstitutionTemplateLiteral => {
                identifiers.add(node.text());
            }
            _ => {}
        }
        node.for_each_child(&mut |child| collect(child, identifiers));
        false
    }

    let mut identifiers: Set<&'static str> = Set::default();
    collect(source_file.as_node(), &mut identifiers);
    identifiers
}

impl NodeFactory {
    pub fn update_source_file(
        &self,
        node: P<Node>,
        statements: P<NodeList>,
        end_of_file_token: P<Node>,
    ) -> P<Node> {
        let file = node.as_source_file();
        if statements != file.statements || end_of_file_token != file.end_of_file_token {
            let updated = self.new_source_file(
                file.parse_options.clone(),
                file.text,
                statements,
                end_of_file_token,
            );
            updated.as_source_file().copy_from(file);
            return update_node(updated, node, &self.hooks);
        }
        node
    }
}

// ast.go:2441
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TokenCacheKey {
    parent: P<Node>,
    loc: TextRange,
}

impl SourceFile {
    // ast.go:2909
    pub fn get_or_create_token(
        &self,
        kind: Kind,
        pos: u32,
        end: u32,
        parent: P<Node>,
        flags: TokenFlags,
    ) -> P<Node> {
        let _region = self.owner_region();
        let mut token_cache = self.token_cache.lock().unwrap();
        let loc = TextRange::new(pos, end);
        let key = TokenCacheKey { parent, loc };
        match token_cache.entry(key) {
            Entry::Occupied(entry) => {
                let token = *entry.get();
                assert!(
                    token.kind() == kind,
                    "Token cache mismatch: {:?} != {:?}",
                    token.kind(),
                    kind
                );
                token
            }
            Entry::Vacant(entry) => {
                assert!(
                    !parent.flags().intersects(NodeFlags::Reparsed),
                    "Cannot create token from reparsed node of kind {:?}",
                    parent.kind()
                );
                let token = create_token(kind, self, pos, end, flags);
                token.set_loc(loc);
                token.set_parent(Some(parent));
                entry.insert(token);
                token
            }
        }
    }
}

impl SourceFile {
    // ast.go:2973
    pub fn get_declaration_map(&self) -> &FxHashMap<String, Vec<P<Node>>> {
        if let Some(m) = self.declaration_map.get() {
            return m;
        }
        let _region = self.owner_region();
        self.declaration_map.get_or_init(|| self.compute_declaration_map())
    }

    // Go `GetOrComputeData(sourceFileSymbolIndexKey, ...)`: the API's file-owned symbols by id, computed once.
    pub fn get_or_compute_api_symbol_index(
        &self,
        compute: impl FnOnce() -> FxHashMap<u64, P<Symbol>>,
    ) -> &FxHashMap<u64, P<Symbol>> {
        if let Some(m) = self.api_symbol_index.get() {
            return m;
        }
        let _region = self.owner_region();
        self.api_symbol_index.get_or_init(compute)
    }

    // ast.go:2982
    fn compute_declaration_map(&self) -> FxHashMap<String, Vec<P<Node>>> {
        fn add_declaration(result: &mut FxHashMap<String, Vec<P<Node>>>, declaration: P<Node>) {
            let name = get_declaration_name(declaration);
            if !name.is_empty() {
                result.entry(name).or_default().push(declaration);
            }
        }

        fn visit(result: &mut FxHashMap<String, Vec<P<Node>>>, node: P<Node>) -> bool {
            match node.kind() {
                Kind::FunctionDeclaration
                | Kind::FunctionExpression
                | Kind::MethodDeclaration
                | Kind::MethodSignature => {
                    let declaration_name = get_declaration_name(node);
                    if !declaration_name.is_empty() {
                        let declarations = result.entry(declaration_name).or_default();
                        let last_declaration = declarations.last().copied();
                        // Check whether this declaration belongs to an "overload group".
                        if let Some(last_declaration) = last_declaration
                            .filter(|l| node.parent() == l.parent() && node.symbol() == l.symbol())
                        {
                            // Overwrite the last declaration if it was an overload and this one is an implementation.
                            if node.body().is_some() && last_declaration.body().is_none() {
                                *declarations.last_mut().unwrap() = node;
                            }
                        } else {
                            declarations.push(node);
                        }
                    }
                    node.for_each_child(&mut |c| visit(result, c));
                }
                Kind::ClassDeclaration
                | Kind::ClassExpression
                | Kind::InterfaceDeclaration
                | Kind::TypeAliasDeclaration
                | Kind::EnumDeclaration
                | Kind::ModuleDeclaration
                | Kind::ImportEqualsDeclaration
                | Kind::ImportClause
                | Kind::NamespaceImport
                | Kind::GetAccessor
                | Kind::SetAccessor
                | Kind::TypeLiteral => {
                    add_declaration(result, node);
                    node.for_each_child(&mut |c| visit(result, c));
                }
                Kind::ImportSpecifier | Kind::ExportSpecifier => {
                    if node.property_name().is_some() {
                        add_declaration(result, node);
                    }
                }
                Kind::Parameter | Kind::VariableDeclaration | Kind::BindingElement => {
                    // Only consider parameter properties
                    if node.kind() == Kind::Parameter
                        && !has_syntactic_modifier(node, ModifierFlags::ParameterPropertyModifier)
                    {
                        return false;
                    }
                    if let Some(name) = node.name() {
                        if is_binding_pattern(name) {
                            name.for_each_child(&mut |c| visit(result, c));
                        } else {
                            if let Some(initializer) = node.initializer() {
                                visit(result, initializer);
                            }
                            add_declaration(result, node);
                        }
                    }
                }
                Kind::EnumMember | Kind::PropertyDeclaration | Kind::PropertySignature => {
                    add_declaration(result, node);
                }
                Kind::ExportDeclaration => {
                    // Handle named exports case e.g.:
                    //    export {a, b as B} from "mod";
                    if let Some(export_clause) = node.as_export_declaration().export_clause {
                        if is_named_exports(export_clause) {
                            for &element in export_clause.elements() {
                                visit(result, element);
                            }
                        } else {
                            visit(result, export_clause.name().unwrap());
                        }
                    }
                }
                Kind::ImportDeclaration => {
                    if let Some(import_clause) = node.as_import_declaration().import_clause {
                        // Handle default import case e.g.:
                        //    import d from "mod";
                        if let Some(name) = import_clause.name() {
                            add_declaration(result, name);
                        }
                        // Handle named bindings in imports e.g.:
                        //    import * as NS from "mod";
                        //    import {a, b as B} from "mod";
                        if let Some(named_bindings) =
                            import_clause.as_import_clause().named_bindings
                        {
                            if named_bindings.kind() == Kind::NamespaceImport {
                                add_declaration(result, named_bindings);
                            } else {
                                for &element in named_bindings.elements() {
                                    visit(result, element);
                                }
                            }
                        }
                    }
                }
                Kind::BinaryExpression => {
                    if matches!(
                        get_assignment_declaration_kind(node),
                        JSDeclarationKind::ExportsProperty
                            | JSDeclarationKind::ThisProperty
                            | JSDeclarationKind::Property
                    ) {
                        add_declaration(result, node);
                    }
                    node.for_each_child(&mut |c| visit(result, c));
                }
                _ => {
                    node.for_each_child(&mut |c| visit(result, c));
                }
            }
            false
        }

        let mut result: FxHashMap<String, Vec<P<Node>>> = FxHashMap::default();

        self.as_node().for_each_child(&mut |c| visit(&mut result, c));
        result
    }
}

// ast.go:2940
// `kind` should be a token kind.
// Go keeps one lazily created factory per file (`tokenFactory`); a NodeFactory handle is not `Sync`, and a factory
// carries no state that tokens observe, so each call uses a fresh default factory.
fn create_token(kind: Kind, file: &SourceFile, pos: u32, end: u32, flags: TokenFlags) -> P<Node> {
    let token_factory = NodeFactory::default();
    let text: &'static str = &file.text[pos as usize..end as usize];
    match kind {
        Kind::NumericLiteral => token_factory.new_numeric_literal(text, flags),
        Kind::BigIntLiteral => token_factory.new_big_int_literal(text, flags),
        Kind::StringLiteral => token_factory.new_string_literal(text, flags),
        Kind::JsxText | Kind::JsxTextAllWhiteSpaces => {
            token_factory.new_jsx_text(text, kind == Kind::JsxTextAllWhiteSpaces)
        }
        Kind::RegularExpressionLiteral => token_factory.new_regular_expression_literal(text, flags),
        Kind::NoSubstitutionTemplateLiteral => {
            token_factory.new_no_substitution_template_literal(text, flags)
        }
        Kind::TemplateHead => token_factory.new_template_head(text, "" /*rawText*/, flags),
        Kind::TemplateMiddle => token_factory.new_template_middle(text, "" /*rawText*/, flags),
        Kind::TemplateTail => token_factory.new_template_tail(text, "" /*rawText*/, flags),
        Kind::Identifier => token_factory.new_identifier(text),
        Kind::PrivateIdentifier => token_factory.new_private_identifier(text),
        _ => token_factory.new_token(kind), // Punctuation and keywords
    }
}

pub trait SourceFileLike {
    fn text(&self) -> &str;
    fn ecma_line_map(&self) -> &[u32];
}

impl SourceFileLike for SourceFile {
    fn text(&self) -> &str {
        self.text
    }
    fn ecma_line_map(&self) -> &[u32] {
        SourceFile::ecma_line_map(self)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CommentRange {
    pub text_range: TextRange,
    pub kind: Kind,
    pub has_trailing_new_line: bool,
}

impl CommentRange {
    #[inline]
    pub fn pos(&self) -> u32 {
        self.text_range.pos().expect_source("comments have source positions")
    }
    #[inline]
    pub fn end(&self) -> u32 {
        self.text_range.end().expect_source("comments have source positions")
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileReference {
    pub text_range: TextRange,
    pub file_name: String,
    pub resolution_mode: ResolutionMode,
    pub preserve: bool,
}

impl FileReference {
    #[inline]
    pub fn pos(&self) -> u32 {
        self.text_range.pos().expect_source("file references have source positions")
    }
    #[inline]
    pub fn end(&self) -> u32 {
        self.text_range.end().expect_source("file references have source positions")
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PragmaArgument {
    pub text_range: TextRange,
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug)]
pub struct Pragma {
    pub comment_range: CommentRange,
    pub name: String,
    pub args: FxHashMap<String, PragmaArgument>,
}

pub type PragmaKindFlags = u8;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PragmaArgumentSpecification {
    name: &'static str,
    optional: bool,
    capture_span: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PragmaSpecification {
    args: &'static [PragmaArgumentSpecification],
    kind: PragmaKindFlags,
}

// Hand-written visitor implementations for nodes with runtime-dependent
// child ordering. Generated code in generated.rs delegates to these.

pub(crate) fn for_each_child_jsdoc_parameter_or_property_tag<V: FnMut(P<Node>) -> bool + ?Sized>(
    node: &JSDocParameterOrPropertyTag,
    v: &mut V,
) -> bool {
    v(node.tag_name())
        || (node.is_name_first && (v(node.name) || visit(v, node.type_expression)))
        || (!node.is_name_first && (visit(v, node.type_expression) || v(node.name)))
        || visit_node_list(v, node.comment())
}

pub(crate) fn visit_each_child_jsdoc_parameter_or_property_tag(
    node: &JSDocParameterOrPropertyTag,
    n: P<Node>,
    v: &mut NodeVisitor,
) -> P<Node> {
    let tag_name = v.visit_node_hooked(Some(node.tag_name())).unwrap();
    let name = v.visit_node_hooked(Some(node.name)).unwrap();
    let type_expression = v.visit_node_hooked(node.type_expression);
    let comment = v.visit_nodes_hooked(node.comment());
    v.factory.update_jsdoc_parameter_or_property_tag(
        n,
        tag_name,
        name,
        node.is_bracketed,
        type_expression,
        node.is_name_first,
        comment,
    )
}

impl NodeFactory {
    #[expect(
        clippy::unused_self,
        reason = "The factory keeps the same instance API for node construction, hooks and arena lifecycle operations."
    )]
    pub fn release_arenas(&self) {}
}
