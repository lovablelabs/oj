use crate::P;
use rustc_hash::FxHashMap;
use std::cell::{Cell, RefCell};

// Links store
//
// Values are arena-allocated and returned as `P<V>`; callers mutate them through `Cell` fields.
pub struct LinkStore<K: 'static, V: 'static> {
    entries: RefCell<FxHashMap<P<K>, P<V>>>,
    /// Values go to the thread's scratch region (`P::new_scratch`): the store of an object that dies with it.
    scratch: bool,
}

impl<K, V> Default for LinkStore<K, V> {
    fn default() -> Self {
        LinkStore { entries: RefCell::new(FxHashMap::default()), scratch: false }
    }
}

impl<K, V> LinkStore<K, V> {
    /// A store whose values are allocated in the thread's scratch region when one is entered (see `scratch`).
    pub fn new_scratch(scratch: bool) -> Self {
        LinkStore { entries: RefCell::new(FxHashMap::default()), scratch }
    }

    /// Forgets every entry (and releases the table, so no stale entry stays in its memory).
    pub fn clear(&self) {
        *self.entries.borrow_mut() = FxHashMap::default();
    }
}

impl<K, V: Default> LinkStore<K, V> {
    pub fn get(&self, key: P<K>) -> P<V> {
        if let Some(&value) = self.entries.borrow().get(&key) {
            return value;
        }
        let value = P::new_in(self.scratch, V::default());
        self.entries.borrow_mut().insert(key, value);
        value
    }
}

impl<K, V> LinkStore<K, V> {
    pub fn has(&self, key: P<K>) -> bool {
        self.entries.borrow().contains_key(&key)
    }

    pub fn try_get(&self, key: P<K>) -> Option<P<V>> {
        self.entries.borrow().get(&key).copied()
    }
}

const LINK_PAGE_SHIFT: u64 = 8;
pub const LINK_PAGE_SIZE: usize = 1 << LINK_PAGE_SHIFT;
const LINK_PAGE_MASK: u64 = LINK_PAGE_SIZE as u64 - 1;
const MAX_PAGE_COUNT: u64 = 65536;

// Implements a sparse-array-like structure for storing elements keyed by dense uint64 keys. Pages hold
// optional handles to separately allocated values, so an untouched slot has no links and existing links keep
// their address as pages and the index grow. The map preserves support for keys outside the dense range.
pub struct PagedLinkStore<V: 'static> {
    page_map: RefCell<FxHashMap<u64, &'static [Cell<Option<P<V>>>]>>,
    page_list: RefCell<Vec<Option<&'static [Cell<Option<P<V>>>]>>>,
}

impl<V> Default for PagedLinkStore<V> {
    fn default() -> Self {
        PagedLinkStore {
            page_map: RefCell::new(FxHashMap::default()),
            page_list: RefCell::new(Vec::new()),
        }
    }
}

impl<V: Default> PagedLinkStore<V> {
    fn new_page() -> &'static [Cell<Option<P<V>>>] {
        crate::alloc_vec(std::iter::repeat_with(|| Cell::new(None)).take(LINK_PAGE_SIZE).collect())
    }

    pub fn get(&self, key: u64) -> P<V> {
        let page_index = key >> LINK_PAGE_SHIFT;
        let page = if page_index < MAX_PAGE_COUNT {
            let mut list = self.page_list.borrow_mut();
            if page_index as usize >= list.len() {
                // Grow the length of the list to pageIndex+1
                list.resize(page_index as usize + 1, None);
            }
            if let Some(page) = list[page_index as usize] {
                page
            } else {
                let page = Self::new_page();
                list[page_index as usize] = Some(page);
                page
            }
        } else {
            let mut map = self.page_map.borrow_mut();
            *map.entry(page_index).or_insert_with(Self::new_page)
        };
        let slot = &page[(key & LINK_PAGE_MASK) as usize];
        if let Some(value) = slot.get() {
            return value;
        }
        let value = P::new(V::default());
        slot.set(Some(value));
        value
    }
}

impl<V> PagedLinkStore<V> {
    pub fn has(&self, key: u64) -> bool {
        self.try_get(key).is_some()
    }

    pub fn try_get(&self, key: u64) -> Option<P<V>> {
        let page_index = key >> LINK_PAGE_SHIFT;
        let page = if page_index < MAX_PAGE_COUNT {
            self.page_list.borrow().get(page_index as usize).copied().flatten()
        } else {
            self.page_map.borrow().get(&page_index).copied()
        };
        let page = page?;
        page[(key & LINK_PAGE_MASK) as usize].get()
    }
}
