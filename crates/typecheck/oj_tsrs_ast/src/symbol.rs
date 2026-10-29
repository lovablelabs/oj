use std::hash::BuildHasher;
use std::sync::atomic::AtomicU32;

use hashbrown::HashTable;
use rustc_hash::FxBuildHasher;
use tsrs_core::{FrozenCell, OwnedCell, OwnedTaggedStrCell, P, PKey, ThinSlice};

use crate::ast::{Node, SourceFile};
use crate::checkflags::CheckFlags;
use crate::symbolflags::SymbolFlags;
use crate::*;

// Symbol
//
// Go's `Symbol` holds `Members`, `Exports` and `ExportSymbol` inline. Few symbols have any of them (on the private monorepo 5%
// of 15.3M: binder symbols of classes, interfaces, modules and exported locals; almost no transient symbols), so
// they live in a tail allocated on the first write of a non-nil value (`members()` / `set_members()` & co.).
// Reads of an absent tail return nil, like the unset Go field. The tail shares a field with `parent`
// (`parent_or_tables`): the field holds the parent until a tail exists, then the tail (which holds the parent).
// `ValueDeclaration` is the first declaration in 82% of the symbols and nil in 17.5% (on the private monorepo:
// 10.88M / 2.31M of 13.2M; another node in 23K), so a bit says "the first declaration" and only another node is
// kept in the tail (`value_declaration()` / `set_value_declaration()`; a declarations write that replaces the
// first declaration moves it to the tail first). Both bits (`TAG_TABLES`, `TAG_VALUE_FIRST`) are the tag bits of
// the name word (`OwnedTaggedStrCell`: pointer, length and tags in one word).
// Ordinary symbols allocate a header and an inline SymbolData payload. Instantiated symbols allocate
// only the header and store their ordinary data owner's key in parent_or_tables. The private high bit
// of check_flags_and_kind selects that layout independently of the semantic flags and symbol id.

#[repr(C, align(8))]
pub struct Symbol {
    pub flags: OwnedCell<SymbolFlags>,
    check_flags_and_kind: OwnedCell<u32>,
    pub(crate) id: AtomicU32, // Low 31 bits for ordinary ids; high bit tags Go's reserved block range.
    pub parent_or_tables: OwnedCell<PKey>, // parent, table tail, or shared data owner; 0 = none
}

#[repr(C)]
#[derive(Default)]
pub struct SymbolData {
    name: OwnedTaggedStrCell,
    // Go slice: shared by copies, replaced (not mutated) on append. ThinSlice keeps this 8 bytes
    // with compressed and plain pointers, with a long form for unusually large declaration lists.
    pub declarations: OwnedCell<ThinSlice<P<Node>>>,
}

#[repr(C)]
struct SymbolWithData {
    symbol: Symbol,
    data: SymbolData,
}

#[derive(Default)]
pub struct SymbolTables {
    parent: OwnedCell<Option<P<Symbol>>>,
    members: OwnedCell<Option<P<SymbolTable>>>,
    exports: OwnedCell<Option<P<SymbolTable>>>,
    export_symbol: OwnedCell<Option<P<Symbol>>>,
    value_declaration: OwnedCell<Option<P<Node>>>, // when it is not the first declaration
    // tsrs-only: a member list of one of the symbol's declarations that is parsed and bound on first use
    // (`lazylist`); `members` and `exports` force it first. Written by the binder of the file only.
    lazy: OwnedCell<Option<P<crate::lazylist::LazyNodeList>>>,
}

pub const SHARED_DATA: u32 = 1 << 31;
const _: () = assert!(CheckFlags::all().bits() & SHARED_DATA == 0);
const _: () = assert!(std::mem::offset_of!(SymbolWithData, symbol) == 0);
const _: () = assert!(std::mem::offset_of!(SymbolWithData, data) == std::mem::size_of::<Symbol>());
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<Symbol>() == if tsrs_core::COMPRESSED_PTRS { 16 } else { 24 });
    assert!(
        std::mem::size_of::<SymbolWithData>() == if tsrs_core::COMPRESSED_PTRS { 32 } else { 40 }
    );
};
#[cfg(target_pointer_width = "32")]
const _: () = {
    assert!(std::mem::size_of::<Symbol>() == 16);
    assert!(std::mem::size_of::<SymbolWithData>() == 32);
};

/// `parent_or_tables` holds the `SymbolTables` tail.
const TAG_TABLES: u8 = 1;
/// The value declaration is the first declaration.
const TAG_VALUE_FIRST: u8 = 2;

impl Symbol {
    /// Allocates a fresh symbol (Go `&ast.Symbol{Flags: flags, Name: name}`).
    pub fn new(flags: SymbolFlags, name: &'static str) -> P<Symbol> {
        let allocation = P::new(SymbolWithData {
            symbol: Symbol {
                flags: OwnedCell::new(flags),
                check_flags_and_kind: OwnedCell::new(0),
                id: AtomicU32::new(0),
                parent_or_tables: OwnedCell::new(0),
            },
            data: SymbolData { name: OwnedTaggedStrCell::new(name), ..Default::default() },
        });
        // SAFETY: SymbolWithData is repr(C) with the 8-aligned Symbol header first.
        let s = unsafe { allocation.cast::<Symbol>() };
        if tsrs_core::sharedgraph::on_seed_thread() {
            crate::get_symbol_id(s);
        }
        s
    }

    /// A transient symbol with its own flags, check flags and id, sharing the source's data.
    /// Go `newSharedDataSymbol` stores the same `symbolData` pointer on both symbols.
    pub fn new_shared_data(source: P<Symbol>) -> P<Symbol> {
        let data = if source.shares_data() {
            // SAFETY: a shared symbol stores its live data owner's key here.
            unsafe { P::from_key(source.parent_or_tables.get()) }
        } else {
            source
        };
        let s = P::new(Symbol {
            flags: OwnedCell::new(source.flags() | SymbolFlags::Transient),
            check_flags_and_kind: OwnedCell::new(SHARED_DATA),
            id: AtomicU32::new(0),
            parent_or_tables: OwnedCell::new(data.key()),
        });
        if tsrs_core::sharedgraph::on_seed_thread() {
            crate::get_symbol_id(s);
        }
        s
    }

    #[inline]
    pub fn shares_data(&self) -> bool {
        self.check_flags_and_kind.get() & SHARED_DATA != 0
    }

    /// Inline payload of an ordinary data owner; shared instances never call this on themselves.
    #[inline]
    pub fn own_data(&self) -> &SymbolData {
        debug_assert!(!self.shares_data());
        // SAFETY: only new and new_shared_data construct Symbol. An ordinary header is the first
        // field of SymbolWithData, whose live payload is at this offset in the same arena allocation.
        unsafe {
            tsrs_core::ptr::arena_neighbor(
                self,
                std::mem::offset_of!(SymbolWithData, data) as isize,
            )
        }
    }

    #[inline]
    fn data_owner(&self) -> &Symbol {
        if self.shares_data() {
            let key = self.parent_or_tables.get();
            assert_ne!(key, 0, "instantiated symbol has no shared data");
            // SAFETY: `new_shared_data` stores the key of the live data owner, flattening
            // nested instances. The arena object outlives this instance.
            unsafe { P::<Symbol>::from_key(key) }.get()
        } else {
            self
        }
    }

    #[inline]
    pub fn flags(&self) -> SymbolFlags {
        self.flags.get()
    }
    #[inline]
    pub fn set_flags(&self, flags: SymbolFlags) {
        self.flags.set(flags);
    }
    #[inline]
    pub fn check_flags(&self) -> CheckFlags {
        CheckFlags::from_bits_retain(self.check_flags_and_kind.get() & !SHARED_DATA)
    }
    /// Updates semantic check flags without changing the immutable allocation layout.
    #[inline]
    pub fn set_check_flags(&self, flags: CheckFlags) {
        assert_eq!(flags.bits() & SHARED_DATA, 0, "check flags overlap the symbol layout tag");
        let kind = self.check_flags_and_kind.get() & SHARED_DATA;
        self.check_flags_and_kind.set(flags.bits() | kind);
    }
    #[inline]
    pub fn name(&self) -> &'static str {
        self.data_owner().own_data().name.get()
    }
    #[inline]
    pub fn set_name(&self, name: &'static str) {
        self.data_owner().own_data().name.set(name);
    }
    #[inline]
    pub fn declarations(&self) -> &'static [P<Node>] {
        self.data_owner().own_data().declarations.get().get()
    }
    /// Go `symbol.Declarations = slices.Clone(declarations)`-like: stores a copy.
    #[inline]
    pub fn set_declarations(&self, declarations: &[P<Node>]) {
        self.set_declarations_static(tsrs_core::alloc_slice(declarations));
    }
    /// Go `symbol.Declarations = declarations` (shares the slice).
    pub fn set_declarations_static(&self, declarations: &'static [P<Node>]) {
        let data = self.data_owner();
        let tags = data.own_data().name.tags();
        if tags & TAG_VALUE_FIRST != 0 {
            let value_declaration = data.own_data().declarations.get().get()[0];
            if declarations.first() != Some(&value_declaration) {
                data.own_data().name.set_tags(tags & !TAG_VALUE_FIRST);
                data.tables_for_write().value_declaration.set(Some(value_declaration));
            }
        }
        data.own_data().declarations.set(ThinSlice::new(declarations));
    }
    /// Go `append(symbol.Declarations, declarations...)`.
    pub fn append_declarations(&self, declarations: &[P<Node>]) {
        if declarations.is_empty() {
            return;
        }
        if self.declarations().is_empty() {
            self.set_declarations(declarations);
            return;
        }
        let mut result = Vec::with_capacity(self.declarations().len() + declarations.len());
        result.extend_from_slice(self.declarations());
        result.extend_from_slice(declarations);
        self.set_declarations_static(tsrs_core::alloc_vec(result));
    }
    #[inline]
    #[expect(
        clippy::disallowed_methods,
        reason = "indexing with a bounds check: +0.7% instructions, one checker (notes/mem-small.md)"
    )]
    pub fn value_declaration(&self) -> Option<P<Node>> {
        let data = self.data_owner();
        if data.own_data().name.tags() & TAG_VALUE_FIRST != 0 {
            let declarations = data.own_data().declarations.get().get();
            debug_assert_ne!(declarations, []);
            // SAFETY: the bit is set only while the declarations are non-empty (`set_value_declaration`,
            // `set_declarations_static`).
            return Some(unsafe { *declarations.get_unchecked(0) });
        }
        data.tables()?.value_declaration.get()
    }
    pub fn set_value_declaration(&self, value_declaration: Option<P<Node>>) {
        let data = self.data_owner();
        let tags = data.own_data().name.tags();
        if value_declaration.is_some()
            && data.own_data().declarations.get().get().first().copied() == value_declaration
        {
            data.own_data().name.set_tags(tags | TAG_VALUE_FIRST);
            if let Some(tables) = data.tables() {
                tables.value_declaration.set(None);
            }
            return;
        }
        data.own_data().name.set_tags(tags & !TAG_VALUE_FIRST);
        if value_declaration.is_some() || data.tables().is_some() {
            data.tables_for_write().value_declaration.set(value_declaration);
        }
    }
    #[inline]
    pub fn tables(&self) -> Option<P<SymbolTables>> {
        let data = self.data_owner();
        // SAFETY: with the tag, the field holds the key of the tail made by `tables_for_write` (never freed).
        (data.own_data().name.tags() & TAG_TABLES != 0)
            .then(|| unsafe { P::from_key(data.parent_or_tables.get()) })
    }
    #[inline]
    fn tables_for_write(&self) -> P<SymbolTables> {
        let data = self.data_owner();
        if let Some(tables) = data.tables() {
            tables
        } else {
            let tables = P::new(SymbolTables {
                parent: OwnedCell::new(data.parent()),
                ..Default::default()
            });
            data.parent_or_tables.set(tables.key());
            data.own_data().name.set_tags(data.own_data().name.tags() | TAG_TABLES);
            tables
        }
    }
    #[inline]
    pub fn members(&self) -> Option<P<SymbolTable>> {
        let t = self.tables()?;
        if let Some(lazy) = t.lazy.get() {
            lazy.ensure();
        }
        t.members.get()
    }
    #[inline]
    pub fn set_members(&self, members: Option<P<SymbolTable>>) {
        if members.is_some() || self.tables().is_some() {
            self.tables_for_write().members.set(members);
        }
    }
    #[inline]
    pub fn exports(&self) -> Option<P<SymbolTable>> {
        let t = self.tables()?;
        if let Some(lazy) = t.lazy.get()
            && lazy.fills_exports()
        {
            lazy.ensure();
        }
        t.exports.get()
    }

    /// The pending lazy member list of one of this symbol's declarations (`lazylist`), if any (forced or not).
    #[inline]
    pub fn lazy_list(&self) -> Option<P<crate::lazylist::LazyNodeList>> {
        self.tables()?.lazy.get()
    }

    /// Binder: `list` (a member list of a declaration of this symbol) is bound on first use.
    pub fn set_lazy_list(&self, list: Option<P<crate::lazylist::LazyNodeList>>) {
        if list.is_some() || self.tables().is_some() {
            self.tables_for_write().lazy.set(list);
        }
    }
    #[inline]
    pub fn set_exports(&self, exports: Option<P<SymbolTable>>) {
        if exports.is_some() || self.tables().is_some() {
            self.tables_for_write().exports.set(exports);
        }
    }
    #[inline]
    pub fn parent(&self) -> Option<P<Symbol>> {
        let data = self.data_owner();
        match data.tables() {
            Some(tables) => tables.parent.get(),
            // SAFETY: without the tag, the field is 0 or the key of the parent (a live symbol).
            None => unsafe { P::from_key_opt(data.parent_or_tables.get()) },
        }
    }
    #[inline]
    pub fn set_parent(&self, parent: Option<P<Symbol>>) {
        let data = self.data_owner();
        match data.tables() {
            Some(tables) => tables.parent.set(parent),
            None => data.parent_or_tables.set(P::key_opt(parent)),
        }
    }
    #[inline]
    pub fn export_symbol(&self) -> Option<P<Symbol>> {
        self.tables()?.export_symbol.get()
    }
    #[inline]
    pub fn set_export_symbol(&self, export_symbol: Option<P<Symbol>>) {
        if export_symbol.is_some() || self.tables().is_some() {
            self.tables_for_write().export_symbol.set(export_symbol);
        }
    }

    pub fn is_external_module(&self) -> bool {
        self.flags.get().intersects(SymbolFlags::Module)
            && is_ambient_module_symbol_name(self.name())
    }

    // See comment on `declareModuleMember` in `binder.go`.
    pub fn combined_local_and_export_symbol_flags(&self) -> SymbolFlags {
        if let Some(export_symbol) = self.export_symbol() {
            return self.flags.get() | export_symbol.flags.get();
        }
        self.flags.get()
    }
}

// GetSourceFileOfSymbol returns the owning file of a published binder symbol, or
// nil for a non-file-owned symbol, even if it borrows declarations from a file.
// Ownership recovery walks only the first declaration's AST parents.
pub fn get_source_file_of_symbol(symbol: P<Symbol>) -> Option<P<SourceFile>> {
    let mut symbol = symbol;
    if symbol.flags.get().intersects(SymbolFlags::Transient) {
        return None;
    }
    if symbol.declarations().is_empty() {
        // A class's implicit prototype has no declaration of its own.
        assert!(
            symbol.flags.get().intersects(SymbolFlags::Prototype),
            "File-bound symbol has no declarations"
        );
        let parent = symbol.parent();
        assert!(
            parent.is_some_and(|p| p.flags.get().intersects(SymbolFlags::Class)),
            "Prototype has no declaring class"
        );
        symbol = parent.unwrap();
        assert!(
            !symbol.flags.get().intersects(SymbolFlags::Transient),
            "Prototype parent is not file-bound"
        );
        assert!(!symbol.declarations().is_empty(), "Prototype parent has no declarations");
    }
    let first = symbol.declarations()[0];
    let file = get_source_file_of_node(first);
    assert!(file.is_some(), "File-bound declaration has no source file");
    file
}

// SymbolTable
//
// Go's `SymbolTable` is a map (a reference type). Here it is an arena object handled as `P<SymbolTable>`;
// a Go nil table is `None`. Iteration is in insertion order and returns snapshots.
//
// Representation: the entries in insertion order in a `Vec`, plus a hash index of entry positions once a table
// has more than `SYMBOL_TABLE_LINEAR_MAX` entries (most tables are tiny: on the private monorepo 82% of the 3.9M tables hold at
// most 8 entries); smaller tables are searched linearly, which compares only the entries' stored lengths and hashes
// until one matches (16 rather than 8 entries: -0.03 GiB on the private monorepo, the same instructions). Same observable behavior as the insertion-ordered map it
// replaces (`IndexMap`): `set` of an existing name keeps the entry's position and stored key, `delete` shifts the
// later entries down.
//
// An entry does not store its key: the key is almost always the symbol's own name (on the private monorepo all but 1 of 16.7M
// inserts; symbol names never change), so an entry is one word: the symbol plus a fingerprint of the key (its
// length up to 63 and 12 bits of its hash), which rejects non-matching entries without reading the symbol (see
// `SymbolMapEntry`). A key with other text than its symbol's name is kept in `SymbolMapExtra::odd_keys`. Keys
// are compared by text, so returning the symbol's name where the caller stored an equal string is unobservable.

#[derive(Default)]
pub struct SymbolTable(FrozenCell<SymbolMap>);

const SYMBOL_TABLE_LINEAR_MAX: usize = 16;

#[derive(Default, Clone)]
struct SymbolMap {
    entries: EntryVec,
    extra: ExtraSlot,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<SymbolMap>() == 24);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<SymbolMap>() == 16);

/// `Vec<SymbolMapEntry>` with a `u32` length and capacity (16 bytes instead of 24; 3.5M symbol tables on the private monorepo).
/// Grows like `Vec` (`push` doubles from 4; `reserve_exact` adds exactly).
struct EntryVec {
    ptr: std::ptr::NonNull<SymbolMapEntry>,
    len: u32,
    cap: u32,
}

impl Default for EntryVec {
    fn default() -> Self {
        EntryVec { ptr: std::ptr::NonNull::dangling(), len: 0, cap: 0 }
    }
}

impl EntryVec {
    fn with_capacity(n: usize) -> EntryVec {
        let mut v = EntryVec::default();
        v.reserve_exact(n);
        v
    }

    #[inline]
    fn capacity(&self) -> usize {
        self.cap as usize
    }

    fn layout(cap: usize) -> std::alloc::Layout {
        std::alloc::Layout::array::<SymbolMapEntry>(cap).expect("symbol table too large")
    }

    /// Makes room for at least `additional` more entries, allocating exactly that much when it grows.
    fn reserve_exact(&mut self, additional: usize) {
        let needed = self.len as usize + additional;
        if needed > self.cap as usize {
            self.set_capacity(needed);
        }
    }

    fn set_capacity(&mut self, cap: usize) {
        let cap32 = u32::try_from(cap).expect("symbol table with more than u32::MAX entries");
        let ptr = if self.cap == 0 {
            // SAFETY: `cap` > 0 entries of a non-zero-sized type.
            unsafe { std::alloc::alloc(Self::layout(cap)) }
        } else {
            // SAFETY: `ptr` was allocated with the layout of `self.cap` entries.
            unsafe {
                std::alloc::realloc(
                    self.ptr.as_ptr().cast(),
                    Self::layout(self.cap as usize),
                    Self::layout(cap).size(),
                )
            }
        };
        self.ptr = std::ptr::NonNull::new(ptr.cast())
            .unwrap_or_else(|| std::alloc::handle_alloc_error(Self::layout(cap)));
        self.cap = cap32;
    }

    #[inline]
    fn push(&mut self, e: SymbolMapEntry) {
        if self.len == self.cap {
            self.set_capacity((self.cap as usize * 2).max(4));
        }
        // SAFETY: `len` < `cap`.
        unsafe { self.ptr.as_ptr().add(self.len as usize).write(e) };
        self.len += 1;
    }

    fn remove(&mut self, i: usize) {
        let len = self.len as usize;
        assert!(i < len, "removal index out of bounds");
        // SAFETY: `i` < `len`; the entries are `Copy`.
        unsafe {
            std::ptr::copy(self.ptr.as_ptr().add(i + 1), self.ptr.as_ptr().add(i), len - i - 1);
        };
        self.len -= 1;
    }
}

impl std::ops::Deref for EntryVec {
    type Target = [SymbolMapEntry];
    #[inline]
    fn deref(&self) -> &[SymbolMapEntry] {
        // SAFETY: the first `len` entries are initialized (dangling and empty when `cap` is 0).
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len as usize) }
    }
}

impl std::ops::DerefMut for EntryVec {
    #[inline]
    fn deref_mut(&mut self) -> &mut [SymbolMapEntry] {
        // SAFETY: as in `deref`, and `&mut self` is unique.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len as usize) }
    }
}

impl Clone for EntryVec {
    /// Like `Vec::clone`: capacity equal to the length.
    fn clone(&self) -> EntryVec {
        let mut v = EntryVec::with_capacity(self.len as usize);
        for &e in self.iter() {
            v.push(e);
        }
        v
    }
}

impl Drop for EntryVec {
    fn drop(&mut self) {
        if self.cap != 0 {
            // SAFETY: allocated with the layout of `cap` entries.
            unsafe {
                std::alloc::dealloc(self.ptr.as_ptr().cast(), Self::layout(self.cap as usize));
            };
        }
    }
}

// SAFETY: an `EntryVec` owns its buffer like a `Vec` (entries are plain words).
unsafe impl Send for EntryVec {}
// SAFETY: as for the `Vec` it stands for: through `&EntryVec` the plain-word entries are only read.
unsafe impl Sync for EntryVec {}

/// One word: the symbol in the low 45 bits (`P::pack`), then the odd-key flag (the key is in `odd_keys`), the key length capped at 63 (6 bits)
/// and the top 12 bits of `hash_name(key)`. 8 bytes instead of 16 (pointer + 32-bit hash + length): symbol table
/// entries are 465 MB of capacity on the private monorepo.
#[derive(Clone, Copy)]
struct SymbolMapEntry(u64);

const _: () = assert!(std::mem::size_of::<SymbolMapEntry>() == 8);

/// The key fingerprint stored in an entry (above the address bits): length capped at 63, then 12 hash bits above it.
/// One word, so testing an entry is one shift and one compare.
#[derive(Clone, Copy, PartialEq, Eq)]
struct KeyPrint(u64);

impl KeyPrint {
    #[inline]
    fn len_bits(name: &str) -> u64 {
        name.len().min(SymbolMapEntry::LEN_MASK as usize) as u64
    }

    #[inline]
    fn hash_bits(hash: u32) -> u64 {
        (hash >> (32 - SymbolMapEntry::HASH_BITS)) as u64
    }

    #[inline]
    fn of(name: &str, hash: u32) -> KeyPrint {
        KeyPrint(
            Self::len_bits(name)
                | (Self::hash_bits(hash)
                    << (SymbolMapEntry::HASH_SHIFT - SymbolMapEntry::LEN_SHIFT)),
        )
    }
}

impl SymbolMapEntry {
    const ADDR_BITS: u32 = tsrs_core::PACK_BITS;
    const ADDR_MASK: u64 = (1 << Self::ADDR_BITS) - 1;
    const ODD_BIT: u64 = 1 << Self::ADDR_BITS;
    const LEN_SHIFT: u32 = Self::ADDR_BITS + 1;
    const LEN_MASK: u64 = (1 << 6) - 1;
    const HASH_SHIFT: u32 = Self::LEN_SHIFT + 6;
    const HASH_BITS: u32 = 64 - Self::HASH_SHIFT;

    #[inline]
    fn addr_bits(symbol: P<Symbol>) -> u64 {
        symbol.pack()
    }

    #[inline]
    fn new(symbol: P<Symbol>, print: KeyPrint) -> SymbolMapEntry {
        SymbolMapEntry(Self::addr_bits(symbol) | (print.0 << Self::LEN_SHIFT))
    }

    #[inline]
    fn symbol(self) -> P<Symbol> {
        // SAFETY: the low bits were stored from a live `P<Symbol>` (arena symbols are never freed or moved).
        unsafe { P::unpack(self.0) }
    }

    #[inline]
    fn set_symbol(&mut self, symbol: P<Symbol>) {
        self.0 = self.0 & !Self::ADDR_MASK | Self::addr_bits(symbol);
    }

    #[inline]
    fn is_odd(self) -> bool {
        self.0 & Self::ODD_BIT != 0
    }

    #[inline]
    fn print(self) -> KeyPrint {
        // The length and hash bits are the top bits of the word.
        const { assert!(SymbolMapEntry::HASH_SHIFT + SymbolMapEntry::HASH_BITS == 64) };
        KeyPrint(self.0 >> Self::LEN_SHIFT)
    }
}

#[derive(Default, Clone)]
struct SymbolMapExtra {
    index: Option<HashTable<u32>>, // positions in `entries`; present once len > SYMBOL_TABLE_LINEAR_MAX
    odd_keys: Vec<(u32, &'static str)>, // (position, key) of the entries whose key is not their symbol's name
}

/// `SymbolMap::extra`: one word holding either a boxed `SymbolMapExtra` (an even address) or, in a table without
/// one (a linear table whose keys are all their symbols' names: nearly every table), a 64-bit Bloom filter of its
/// keys' hashes (two bits per key) whose bit 0 is set as the tag. A lookup the filter rejects returns without
/// reading the entries, which are a second cache line: over half of all lookups are misses in small tables
/// (a property lookup tries each type on the apparent-type chain, down to `Object`'s members), and the entries of
/// a table looked up once in a while are rarely in cache. Deleting a key leaves its bits set (a superset is still
/// a valid filter).
struct ExtraSlot(*mut SymbolMapExtra);

impl ExtraSlot {
    const EMPTY_FILTER: usize = 1;

    #[inline]
    fn boxed(extra: SymbolMapExtra) -> ExtraSlot {
        ExtraSlot(Box::into_raw(Box::new(extra)))
    }

    #[inline]
    fn is_filter(&self) -> bool {
        self.0.addr() & 1 != 0
    }

    #[inline]
    fn get(&self) -> Option<&SymbolMapExtra> {
        // SAFETY: an even word is the pointer `boxed` created, owned by this slot.
        (!self.is_filter()).then(|| unsafe { &*self.0 })
    }

    #[inline]
    fn get_mut(&mut self) -> Option<&mut SymbolMapExtra> {
        // SAFETY: as in `get`, and `&mut self` is unique.
        (!self.is_filter()).then(|| unsafe { &mut *self.0 })
    }

    /// The boxed extra, created (dropping the filter) when the slot holds a filter.
    fn get_or_insert(&mut self) -> &mut SymbolMapExtra {
        if self.is_filter() {
            *self = ExtraSlot::boxed(SymbolMapExtra::default());
        }
        self.get_mut().unwrap()
    }

    #[inline]
    fn filter_bits(hash: u32) -> usize {
        (1usize << (hash & (usize::BITS - 1))) | (1usize << ((hash >> 6) & (usize::BITS - 1)))
    }

    /// False if no key with this hash is in a table whose slot holds a filter.
    #[inline]
    fn may_contain(&self, hash: u32) -> bool {
        let bits = Self::filter_bits(hash);
        !self.is_filter() || self.0.addr() & bits == bits
    }

    #[inline]
    fn add_to_filter(&mut self, hash: u32) {
        if self.is_filter() {
            self.0 = std::ptr::without_provenance_mut(self.0.addr() | Self::filter_bits(hash));
        }
    }
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(usize::BITS == 64);

impl Default for ExtraSlot {
    #[inline]
    fn default() -> ExtraSlot {
        ExtraSlot(std::ptr::without_provenance_mut(Self::EMPTY_FILTER))
    }
}

impl Clone for ExtraSlot {
    fn clone(&self) -> ExtraSlot {
        match self.get() {
            Some(extra) => ExtraSlot::boxed(extra.clone()),
            None => ExtraSlot(self.0),
        }
    }
}

impl Drop for ExtraSlot {
    fn drop(&mut self) {
        if !self.is_filter() {
            // SAFETY: an even word is the pointer `boxed` created, owned by this slot.
            drop(unsafe { Box::from_raw(self.0) });
        }
    }
}

// SAFETY: an `ExtraSlot` owns its `SymbolMapExtra` like a `Box` (or holds plain bits).
unsafe impl Send for ExtraSlot {}
// SAFETY: as for the `Box` it stands for: `SymbolMapExtra` has no interior mutability, and through `&ExtraSlot` it
// is only read.
unsafe impl Sync for ExtraSlot {}

/// A symbol name's 32-bit hash: one widening multiply over the first and last bytes (names of up to 16 bytes, nearly
/// all of them), the string hasher beyond that. Every lookup hashes its name, so this is kept to a few instructions.
#[inline]
fn hash_name(name: &str) -> u32 {
    let b = name.as_bytes();
    let len = b.len();
    let (x, y) = if len >= 8 {
        if len > 16 {
            let h = FxBuildHasher.hash_one(name);
            return (h ^ (h >> 32)) as u32;
        }
        (
            u64::from_le_bytes(b[..8].try_into().unwrap()),
            u64::from_le_bytes(b[len - 8..].try_into().unwrap()),
        )
    } else if len >= 4 {
        (
            u32::from_le_bytes(b[..4].try_into().unwrap()) as u64,
            u32::from_le_bytes(b[len - 4..].try_into().unwrap()) as u64,
        )
    } else if len > 0 {
        (b[0] as u64, ((b[len - 1] as u64) << 8) | b[len / 2] as u64)
    } else {
        (0, 0)
    };
    // The two constants are AArch64 logical immediates (one `eor` each, no four-instruction load).
    let m = ((x ^ 0x5555_5555_5555_5555) as u128)
        .wrapping_mul((y ^ 0x3333_3333_3333_3333 ^ len as u64) as u128);
    let h = (m as u64) ^ ((m >> 64) as u64);
    (h ^ (h >> 32)) as u32
}

/// `a == b`, decided without reading the bytes when both are the same string: over 40% of lookup hits use the very
/// string the symbol was named with (an instantiated property looked up by its declaration's name), and an insert
/// nearly always stores a symbol under its own name.
#[inline]
fn same_text(a: &str, b: &str) -> bool {
    a.len() == b.len() && (std::ptr::eq(a.as_ptr(), b.as_ptr()) || a.as_bytes() == b.as_bytes())
}

/// `a == b` for two strings of the same length of at most 16 bytes, with the loads `hash_name` makes (no call).
#[inline]
fn short_text_eq(a: &[u8], b: &[u8]) -> bool {
    let len = a.len();
    debug_assert!(len == b.len() && len <= 16);
    if len >= 8 {
        a.first_chunk::<8>() == b.first_chunk::<8>() && a.last_chunk::<8>() == b.last_chunk::<8>()
    } else if len >= 4 {
        a.first_chunk::<4>() == b.first_chunk::<4>() && a.last_chunk::<4>() == b.last_chunk::<4>()
    } else {
        len == 0 || a[0] == b[0] && a[len / 2] == b[len / 2] && a[len - 1] == b[len - 1]
    }
}

/// `SymbolMap::key` from the parts it reads (for the index's rehash, which borrows the index).
#[inline]
fn key_at(entries: &[SymbolMapEntry], odd_keys: &[(u32, &'static str)], i: usize) -> &'static str {
    let e = entries[i];
    if !e.is_odd() {
        return e.symbol().name();
    }
    odd_keys.iter().find(|&&(j, _)| j as usize == i).unwrap().1
}

/// The index's hash of an entry: hashbrown takes the bucket from the low bits (the name hash's low bits) and its
/// 7-bit tag from the top bits, here name hash bits 13-19, which neither the bucket of a table below 8192 buckets
/// nor the entry fingerprint (bits 20-31) uses. One shifted `orr`, no constant to load.
#[inline]
fn index_hash(hash: u32) -> u64 {
    let h = hash as u64;
    h | (h << 44)
}

impl SymbolMap {
    fn with_capacity(n: usize) -> SymbolMap {
        let extra = if n > SYMBOL_TABLE_LINEAR_MAX {
            ExtraSlot::boxed(SymbolMapExtra {
                index: Some(HashTable::with_capacity(n)),
                odd_keys: Vec::new(),
            })
        } else {
            ExtraSlot::default()
        };
        SymbolMap { entries: EntryVec::with_capacity(n), extra }
    }

    #[inline]
    fn index(&self) -> Option<&HashTable<u32>> {
        self.extra.get()?.index.as_ref()
    }

    /// The stored key of entry `i`.
    #[inline]
    fn key(&self, i: usize) -> &'static str {
        let e = self.entries[i];
        if !e.is_odd() {
            return e.symbol().name();
        }
        key_at(&self.entries, &self.extra.get().unwrap().odd_keys, i)
    }

    /// Whether entry `i`'s key is `name` (whose fingerprint is `print`).
    #[inline]
    fn entry_matches(&self, i: usize, name: &str, print: KeyPrint) -> bool {
        self.entries[i].print() == print && same_text(self.key(i), name)
    }

    /// The hash of entry `i`'s key (rehashing the index, removing from it).
    #[inline]
    fn entry_hash(&self, i: usize) -> u32 {
        hash_name(self.key(i))
    }

    /// The position of `name`. Over half of all lookups end at the filter (a miss in a small table), so that test is
    /// all this function does before the search, which is out of line: with the search inlined, every call saved
    /// the search's registers first.
    #[inline]
    fn position(&self, name: &str) -> Option<usize> {
        self.position_hashed(name, hash_name(name))
    }

    /// `position` of a name hashed beforehand (`hash` = `hash_name(name)`).
    #[inline]
    fn position_hashed(&self, name: &str, hash: u32) -> Option<usize> {
        if !self.extra.may_contain(hash) {
            return None;
        }
        self.search(name, hash)
    }

    /// `position` past the filter (`hash` = `hash_name(name)`): the first entry with the key's fingerprint, returned
    /// at once when its key has the looked-up text and at most 16 bytes; anything else goes to `search_text`. Nearly
    /// every search ends here (a miss, or a hit on a short name), and without a call on this path the function saves
    /// no registers.
    #[inline(never)]
    fn search(&self, name: &str, hash: u32) -> Option<usize> {
        let print = KeyPrint::of(name, hash);
        let i = match self.index() {
            None => (0..self.entries.len()).find(|&i| self.entries[i].print() == print)?,
            Some(index) => *index
                .find(index_hash(hash), |&i| self.entries[i as usize].print() == print)?
                as usize,
        };
        let e = self.entries[i];
        if !e.is_odd() {
            let key = e.symbol().name();
            if key.len() == name.len()
                && (std::ptr::eq(key.as_ptr(), name.as_ptr())
                    || name.len() <= 16 && short_text_eq(key.as_bytes(), name.as_bytes()))
            {
                return Some(i);
            }
        }
        self.search_text(name, hash, i)
    }

    /// `search` from candidate `i` (the first entry with the key's fingerprint) on, comparing the text. Keys are
    /// unique, so a candidate with the same text is the answer; after a fingerprint collision the linear scan goes on
    /// past `i` and the index is probed again.
    #[inline(never)]
    fn search_text(&self, name: &str, hash: u32, i: usize) -> Option<usize> {
        let print = KeyPrint::of(name, hash);
        if same_text(self.key(i), name) {
            return Some(i);
        }
        match self.index() {
            None => (i + 1..self.entries.len()).find(|&j| self.entry_matches(j, name, print)),
            Some(index) => index
                .find(index_hash(hash), |&j| self.entry_matches(j as usize, name, print))
                .map(|&j| j as usize),
        }
    }

    fn add_odd_key(&mut self, i: usize, key: &'static str) {
        self.entries[i].0 |= SymbolMapEntry::ODD_BIT;
        self.extra.get_or_insert().odd_keys.push((i as u32, key));
    }

    fn insert(&mut self, name: &'static str, symbol: P<Symbol>) {
        let hash = hash_name(name);
        if let Some(i) = if self.extra.may_contain(hash) { self.search(name, hash) } else { None } {
            // Go keeps the stored key; it is no longer the new symbol's name when that differs.
            self.entries[i].set_symbol(symbol);
            if !self.entries[i].is_odd() && !same_text(symbol.name(), name) {
                self.add_odd_key(i, name);
            }
            return;
        }
        let i = self.entries.len();
        if i == self.entries.capacity() && i >= 8 {
            // Grow by half instead of doubling: most large tables stop growing soon after (member tables,
            // property caches), and the slack of a doubled Vec is the larger part of their memory.
            self.entries.reserve_exact(i / 2);
        } else if i == self.entries.capacity() && i < 2 {
            // Capacity 1, then 2, then doubling from 4 (`push`): over half of the binder's tables hold one or two
            // symbols (locals of small functions, members of small object literals).
            self.entries.reserve_exact(1);
        }
        self.entries.push(SymbolMapEntry::new(symbol, KeyPrint::of(name, hash)));
        self.extra.add_to_filter(hash);
        if !same_text(symbol.name(), name) {
            self.add_odd_key(i, name);
        }
        if let Some(SymbolMapExtra { index: Some(index), odd_keys }) = self.extra.get_mut() {
            let entries = &self.entries;
            index.insert_unique(index_hash(hash), i as u32, |&m| {
                index_hash(hash_name(key_at(entries, odd_keys, m as usize)))
            });
        } else if self.entries.len() > SYMBOL_TABLE_LINEAR_MAX {
            self.build_index();
        }
    }

    /// The index of a linear table that has grown past `SYMBOL_TABLE_LINEAR_MAX` entries.
    #[cold]
    #[inline(never)]
    fn build_index(&mut self) {
        let len = self.entries.len();
        let mut index = HashTable::with_capacity(len);
        for j in 0..len {
            index.insert_unique(index_hash(self.entry_hash(j)), j as u32, |&m| {
                index_hash(self.entry_hash(m as usize))
            });
        }
        self.extra.get_or_insert().index = Some(index);
    }

    fn shift_remove(&mut self, name: &str) {
        let Some(i) = self.position(name) else {
            return;
        };
        let hash = self.entry_hash(i);
        if let Some(extra) = self.extra.get_mut() {
            if let Some(index) = &mut extra.index {
                if let Ok(entry) = index.find_entry(index_hash(hash), |&j| j as usize == i) {
                    entry.remove();
                }
                for j in index.iter_mut() {
                    if *j as usize > i {
                        *j -= 1;
                    }
                }
            }
            extra.odd_keys.retain(|&(j, _)| j as usize != i);
            for (j, _) in &mut extra.odd_keys {
                if *j as usize > i {
                    *j -= 1;
                }
            }
        }
        self.entries.remove(i);
    }

    fn pairs(&self) -> Vec<(&'static str, P<Symbol>)> {
        (0..self.entries.len()).map(|i| (self.key(i), self.entries[i].symbol())).collect()
    }
}

/// A name with its hash, for looking one name up in several tables (a property lookup's own members, then
/// `Function`'s and `Object`'s) and testing it against a `NameFilter` without hashing it again for each.
#[derive(Clone, Copy)]
pub struct HashedName<'a> {
    name: &'a str,
    hash: u32,
}

impl<'a> HashedName<'a> {
    #[inline]
    pub fn new(name: &'a str) -> HashedName<'a> {
        HashedName { name, hash: hash_name(name) }
    }

    /// The name's 32-bit hash (the one symbol tables use).
    #[inline]
    pub fn hash(&self) -> u32 {
        self.hash
    }
}

/// Rejects names absent from every table in a group.
///
/// A 256-bit Bloom filter of the keys of a few tables taken together (two bits per key, other hash bits than
/// a table's own filter), for a lookup that tries the same tables for every name: a name it rejects is a key
/// of none of them.
///
/// It holds the keys the tables had when they were added; the owner drops it when they may change.
#[derive(Clone, Copy, Default)]
pub struct NameFilter([u64; 4]);

impl NameFilter {
    #[inline]
    fn bit_positions(hash: u32) -> [u32; 2] {
        [(hash >> 12) & 255, (hash >> 20) & 255]
    }

    pub fn add_keys(&mut self, table: &SymbolTable) {
        let m = table.0.borrow();
        for i in 0..m.entries.len() {
            for b in Self::bit_positions(m.entry_hash(i)) {
                self.0[(b >> 6) as usize] |= 1 << (b & 63);
            }
        }
    }

    #[inline]
    pub fn may_contain(&self, key: HashedName<'_>) -> bool {
        Self::bit_positions(key.hash)
            .iter()
            .all(|&b| self.0[(b >> 6) as usize] & (1 << (b & 63)) != 0)
    }
}

impl SymbolTable {
    /// `lookup` of a name hashed beforehand.
    #[inline]
    pub fn lookup_hashed(&self, key: HashedName<'_>) -> Option<P<Symbol>> {
        let m = self.0.borrow();
        m.position_hashed(key.name, key.hash).map(|i| m.entries[i].symbol())
    }

    /// Go `make(ast.SymbolTable)`.
    pub fn new() -> P<SymbolTable> {
        P::new(SymbolTable::default())
    }

    /// Go `make(ast.SymbolTable, n)`.
    pub fn with_capacity(n: usize) -> P<SymbolTable> {
        P::new(SymbolTable(FrozenCell::new(SymbolMap::with_capacity(n))))
    }

    /// A copy of the table as a value (shared graph: a checker's copy of a frozen table held inline).
    #[must_use]
    pub fn clone_value(&self) -> SymbolTable {
        SymbolTable(FrozenCell::new(self.0.borrow().clone()))
    }

    /// Go `maps.Clone(table)` for a non-nil table.
    pub fn clone_table(&self) -> P<SymbolTable> {
        P::new(SymbolTable(FrozenCell::new(self.0.borrow().clone())))
    }

    /// Empties the table and frees its heap buffers (an arena table no one reads again: arena objects are never
    /// dropped, so its entry buffer and index would otherwise stay allocated).
    pub fn clear_and_free(&self) {
        drop(self.0.replace(SymbolMap::default()));
    }

    /// Go `table[name]`. On a `P<SymbolTable>` receiver `table.get(name)` resolves to `P::get`, so use
    /// `table.lookup(name)` (same thing) there.
    #[inline]
    pub fn get(&self, name: &str) -> Option<P<Symbol>> {
        self.lookup(name)
    }

    #[inline]
    pub fn lookup(&self, name: &str) -> Option<P<Symbol>> {
        let m = self.0.borrow();
        m.position(name).map(|i| m.entries[i].symbol())
    }

    /// `lookup` that also returns the stored key.
    #[inline]
    pub fn lookup_entry(&self, name: &str) -> Option<(&'static str, P<Symbol>)> {
        let m = self.0.borrow();
        m.position(name).map(|i| (m.key(i), m.entries[i].symbol()))
    }

    #[inline]
    pub fn set(&self, name: &'static str, symbol: P<Symbol>) {
        self.0.borrow_mut().insert(name, symbol);
    }

    pub fn delete(&self, name: &str) {
        self.0.borrow_mut().shift_remove(name);
    }

    #[inline]
    pub fn has(&self, name: &str) -> bool {
        self.0.borrow().position(name).is_some()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.borrow().entries.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.borrow().entries.is_empty()
    }

    /// Iterates over a snapshot, so `f` may mutate the table.
    pub fn for_each(&self, mut f: impl FnMut(&'static str, P<Symbol>)) {
        for (name, symbol) in self.entries() {
            f(name, symbol);
        }
    }

    pub fn entries(&self) -> Vec<(&'static str, P<Symbol>)> {
        self.0.borrow().pairs()
    }

    /// The `i`-th entry in insertion order (`entries()[i]` without the snapshot). Iterating `0..len()` with the
    /// length read first visits what `entries()` would, as long as nothing is deleted meanwhile (a `set` replaces in
    /// place or appends past the end).
    #[inline]
    pub fn entry(&self, i: usize) -> (&'static str, P<Symbol>) {
        let m = self.0.borrow();
        (m.key(i), m.entries[i].symbol())
    }

    pub fn keys(&self) -> Vec<&'static str> {
        let m = self.0.borrow();
        (0..m.entries.len()).map(|i| m.key(i)).collect()
    }

    pub fn values(&self) -> Vec<P<Symbol>> {
        self.0.borrow().entries.iter().map(|e| e.symbol()).collect()
    }
}

// Go uses the byte 0xFE, which is invalid UTF-8 and cannot live in a Rust `&str`. The port uses U+007F
// (DEL): a single byte, so `name.as_bytes()[0]` / `name.as_bytes()[1]` checks translate one to one, and it
// cannot occur in an IdentifierName. Translate Go `name[0] == '\xFE'` to
// `name.as_bytes().first() == Some(&InternalSymbolNamePrefixByte)`.
pub const INTERNAL_SYMBOL_NAME_PREFIX: &str = "\u{7f}";
pub const INTERNAL_SYMBOL_NAME_PREFIX_BYTE: u8 = 0x7f;

pub const INTERNAL_SYMBOL_NAME_CALL: &str = "\u{7f}call"; // Call signatures
pub const INTERNAL_SYMBOL_NAME_CONSTRUCTOR: &str = "\u{7f}constructor"; // Constructor implementations
pub const INTERNAL_SYMBOL_NAME_NEW: &str = "\u{7f}new"; // Constructor signatures
pub const INTERNAL_SYMBOL_NAME_INDEX: &str = "\u{7f}index"; // Index signatures
pub const INTERNAL_SYMBOL_NAME_EXPORT_STAR: &str = "\u{7f}export"; // Module export * declarations
pub const INTERNAL_SYMBOL_NAME_GLOBAL: &str = "\u{7f}global"; // Global self-reference
pub const INTERNAL_SYMBOL_NAME_MISSING: &str = "\u{7f}missing"; // Indicates missing symbol
pub const INTERNAL_SYMBOL_NAME_TYPE: &str = "\u{7f}type"; // Anonymous type literal symbol
pub const INTERNAL_SYMBOL_NAME_OBJECT: &str = "\u{7f}object"; // Anonymous object literal declaration
pub const INTERNAL_SYMBOL_NAME_JSX_ATTRIBUTES: &str = "\u{7f}jsxAttributes"; // Anonymous JSX attributes object literal declaration
pub const INTERNAL_SYMBOL_NAME_CLASS: &str = "\u{7f}class"; // Unnamed class expression
pub const INTERNAL_SYMBOL_NAME_FUNCTION: &str = "\u{7f}function"; // Unnamed function expression
pub const INTERNAL_SYMBOL_NAME_COMPUTED: &str = "\u{7f}computed"; // Computed property name declaration with dynamic name
pub const INTERNAL_SYMBOL_NAME_ASSIGNMENT_DECLARATION: &str = "\u{7f}assignment"; // Assignment declarations
pub const INTERNAL_SYMBOL_NAME_INSTANTIATION_EXPRESSION: &str = "\u{7f}instantiationExpression"; // Instantiation expressions
pub const INTERNAL_SYMBOL_NAME_IMPORT_ATTRIBUTES: &str = "\u{7f}importAttributes";
pub const INTERNAL_SYMBOL_NAME_EXPORT_EQUALS: &str = "export="; // Export assignment symbol
pub const INTERNAL_SYMBOL_NAME_DEFAULT: &str = "default"; // Default export symbol (technically not wholly internal, but included here for usability)
pub const INTERNAL_SYMBOL_NAME_THIS: &str = "this";
pub const INTERNAL_SYMBOL_NAME_MODULE_EXPORTS: &str = "module.exports";

pub fn symbol_name(symbol: P<Symbol>) -> &'static str {
    if let Some(value_declaration) = symbol.value_declaration()
        && is_private_identifier_class_element_declaration(value_declaration)
    {
        return value_declaration.name().unwrap().text();
    }
    symbol.name()
}

// EscapeAllInternalSymbolNames replaces internal symbol name markers with "__".
pub fn escape_all_internal_symbol_names(name: &str) -> String {
    name.replace(INTERNAL_SYMBOL_NAME_PREFIX, "__")
}

pub fn escape_internal_symbol_name(name: &str) -> String {
    if let Some(rest) = name.strip_prefix(INTERNAL_SYMBOL_NAME_PREFIX) {
        return format!("__{rest}");
    }
    name.to_string()
}

// EscapeSymbolName converts a binder symbol name into its escaped "__String"
// form. Internal names (prefixed with the sentinel) become "__"-prefixed,
// and user names that already begin with "__" gain an extra leading underscore
// so they can be distinguished from internal names.
pub fn escape_symbol_name(name: &str) -> String {
    if let Some(rest) = name.strip_prefix(INTERNAL_SYMBOL_NAME_PREFIX) {
        return format!("__{rest}");
    }
    let b = name.as_bytes();
    if b.len() >= 2 && b[0] == b'_' && b[1] == b'_' {
        return format!("_{name}");
    }
    name.to_string()
}
