//! Port of the pinned `tsc/internal/api/encoder/encoder.go` (hand-written part). The per-kind tables live in
//! `generated.rs`.
//!
//! Ownership: everything returned here is plain bytes or [`NodeIndexTable`], whose `P<Node>` entries are valid
//! only while the region that owns the encoded source file (its parse-cache region or the program/snapshot that
//! keeps it alive) is alive. The codec never extends that lifetime; the session that stores a table must drop it
//! with the snapshot/lease it was built from.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use rustc_hash::FxHashMap;
use tsrs_ast::*;
use tsrs_core::goslices::sort_func;
use tsrs_core::{P, TextPos, binary_search_unique_func};

use crate::format::*;
use crate::generated::{
    children_property_mask, node_common_data, node_data_type, record_extended_data,
    record_node_strings,
};
use crate::msgpack;
use crate::positionmap::PositionMap;
use crate::stringtable::StringTable;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// A node kind the pinned encoder refuses to encode (Go panics; e.g. `SyntheticExpression`).
    Unencodable(Kind),
    /// A section grew past what a uint32 / 24-bit field can address.
    TooLarge(&'static str),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::Unencodable(k) => write!(f, "{k:?} cannot be encoded"),
            EncodeError::TooLarge(what) => {
                write!(f, "encoded {what} exceeds the binary format limits")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

/// Go `NodeIndexTable`: encoder index → node (index 0 is the nil sentinel, `NodeList` records map to `None`).
pub struct NodeIndexTable {
    nodes: Vec<Option<P<Node>>>,
    sorted: OnceLock<Vec<u32>>,
}

impl NodeIndexTable {
    fn new(nodes: Vec<Option<P<Node>>>) -> NodeIndexTable {
        NodeIndexTable { nodes, sorted: OnceLock::new() }
    }

    /// Go `GetIndex`, ported exactly: on the first call the non-nil indices are sorted by node id with Go's
    /// `slices.SortFunc` (pdqsort, `tsrs_core::goslices::sort_func`), assigning ids lazily in comparison
    /// order like `ast.GetNodeId`, then looked up with `core.BinarySearchUniqueFunc`. A node can occur at
    /// several indices (a `JSDoc` comment hosting `@typedef`/`@callback` is encoded under every declaration
    /// it is attached to); which of them is returned is decided by that sort and search exactly as in Go, so
    /// the handles sent to clients match. Returns 0 when the node is not in the table.
    pub fn get_index(&self, node: P<Node>) -> u32 {
        let sorted = self.sorted.get_or_init(|| {
            let mut idx: Vec<u32> = (0..self.nodes.len() as u32)
                .filter(|&i| self.nodes[i as usize].is_some())
                .collect();
            let nodes = &self.nodes;
            sort_func(&mut idx, |&a, &b| {
                let (ia, ib) = (
                    get_node_id(nodes[a as usize].unwrap()),
                    get_node_id(nodes[b as usize].unwrap()),
                );
                ia.cmp(&ib)
            });
            idx
        });
        let target = get_node_id(node);
        let (i, found) = binary_search_unique_func(sorted, |_, &el| {
            get_node_id(self.nodes[el as usize].unwrap()).cmp(&target)
        });
        if found { sorted[i] } else { 0 }
    }

    /// The node at encoder index `index` (`None` for 0, `NodeList` records and out-of-range indices).
    pub fn node(&self, index: u32) -> Option<P<Node>> {
        self.nodes.get(index as usize).copied().flatten()
    }

    #[expect(
        clippy::len_without_is_empty,
        reason = "Index zero is reserved; the table is never empty."
    )]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
}

/// Mutable state threaded through the generated `record_extended_data` dispatch.
pub(crate) struct EncodeContext {
    strs: StringTable<'static>,
    position_map: PositionMap,
    pub(crate) extended_data: Vec<u8>,
    structured_data: Vec<u8>,
}

impl EncodeContext {
    fn utf16(&self, pos: TextPos) -> u32 {
        match pos {
            TextPos::Synthetic => u32::MAX,
            TextPos::Source(pos) => self.position_map.utf8_to_utf16(i64::from(pos.get())) as u32,
        }
    }
}

fn append_u32s(buf: &mut Vec<u8>, values: &[u32]) {
    buf.extend(values.iter().flat_map(|v| v.to_le_bytes()));
}

/// Go `hasModifiers`.
pub(crate) fn has_modifiers(modifiers: Option<P<ModifierList>>) -> bool {
    modifiers.is_some_and(|m| !m.list.nodes().is_empty())
}

/// Presence of an optional child, used by the generated children masks (`!= nil` in Go).
pub(crate) trait Present {
    fn present(&self) -> bool;
}
impl Present for P<Node> {
    fn present(&self) -> bool {
        true
    }
}
impl Present for P<NodeList> {
    fn present(&self) -> bool {
        true
    }
}
impl<T> Present for Option<T> {
    fn present(&self) -> bool {
        self.is_some()
    }
}
pub(crate) fn present<T: Present + Copy>(v: T) -> bool {
    v.present()
}

/// Go `SourceFileHash`: the 128-bit content hash as 32 hex digits (hi then lo).
pub fn source_file_hash(file: &SourceFile) -> String {
    let h = file.hash.get();
    format!("{:016x}{:016x}", (h >> 64) as u64, h as u64)
}

/// `source_file_hash` written into `out` without allocating.
pub fn source_file_hash_into<'a>(file: &SourceFile, out: &'a mut [u8; 32]) -> &'a str {
    let h = file.hash.get();
    for (i, digit) in out.iter_mut().enumerate() {
        *digit = b"0123456789abcdef"[((h >> (124 - 4 * i)) & 0xf) as usize];
    }
    std::str::from_utf8(out).unwrap_or_default()
}

/// Go `SetSourceFileLease`.
pub fn set_source_file_lease(data: &mut [u8], lease: u64) {
    data[HEADER_OFFSET_SOURCE_FILE_LEASE..HEADER_OFFSET_SOURCE_FILE_LEASE + 8]
        .copy_from_slice(&lease.to_le_bytes());
}

/// Go `SetSourceFileID`.
pub fn set_source_file_id(data: &mut [u8], id: u64) {
    data[HEADER_OFFSET_SOURCE_FILE_ID..HEADER_OFFSET_SOURCE_FILE_ID + 8]
        .copy_from_slice(&id.to_le_bytes());
}

fn encode_parse_options(opts: ExternalModuleIndicatorOptions) -> u32 {
    (opts.jsx as u32) | ((opts.force as u32) << 1)
}

/// Go `EncodeSourceFile`.
///
/// Returns the encoded bytes and the node index table (same indices as the bytes). The caller owns caching
/// the table for the source file's lifetime (Go stores it in the file's data map).
pub fn encode_source_file(
    file: &'static SourceFile,
) -> Result<(Vec<u8>, NodeIndexTable), EncodeError> {
    encode_tree(file.as_node(), Some(file))
}

/// Go `EncodeNode`: encodes an arbitrary subtree. `source_file` supplies `JSDoc` and the position map; pass
/// `None` for synthesized nodes (e.g. a checker `typeToTypeNode` result), as the pinned session does.
pub fn encode_node(
    node: P<Node>,
    source_file: Option<&'static SourceFile>,
) -> Result<(Vec<u8>, NodeIndexTable), EncodeError> {
    encode_tree(node, source_file)
}

/// Walks `root` in exactly the order of the pinned encoder: Go `NodeVisitor.VisitEachChild` with the
/// `VisitNodes` and `VisitModifiers` hooks overridden, then the node's `JSDoc`. `enter_node` gets each node,
/// `enter_list` each `NodeList` (modifier lists only when non-empty); each returns the state that `leave`
/// restores after the children.
trait Walker {
    fn enter_node(&mut self, node: P<Node>) -> (u32, u32);
    fn enter_list(&mut self, list: &NodeList) -> (u32, u32);
    fn leave(&mut self, saved: (u32, u32));
}

fn walk_children<W: Walker + 'static>(
    root: P<Node>,
    file: Option<&'static SourceFile>,
    state: Rc<RefCell<W>>,
) {
    let visit: VisitFn = {
        let state = Rc::clone(&state);
        Rc::new(move |v: &mut NodeVisitor, node: P<Node>| -> Option<P<Node>> {
            let saved = state.borrow_mut().enter_node(node);
            v.visit_each_child(Some(node));
            if let Some(file) = file {
                let visit = v.visit.clone().unwrap();
                for &jsdoc in node.jsdoc(Some(file)) {
                    visit(v, jsdoc);
                }
            }
            state.borrow_mut().leave(saved);
            Some(node)
        })
    };
    let visit_list = {
        move |list: &NodeList, v: &mut NodeVisitor| {
            let saved = state.borrow_mut().enter_list(list);
            v.visit_slice(list.nodes());
            state.borrow_mut().leave(saved);
        }
    };
    let visit_list = Rc::new(visit_list);
    let hooks = NodeVisitorHooks {
        visit_nodes: Some({
            let visit_list = Rc::clone(&visit_list);
            Rc::new(move |list: Option<P<NodeList>>, v: &mut NodeVisitor| {
                if let Some(list) = list {
                    visit_list(&list, v);
                }
                list
            })
        }),
        visit_modifiers: Some({
            let visit_list = Rc::clone(&visit_list);
            Rc::new(move |mods: Option<P<ModifierList>>, v: &mut NodeVisitor| {
                if let Some(m) = mods
                    && !m.list.nodes().is_empty()
                {
                    visit_list(&m.list, v);
                }
                mods
            })
        }),
        ..Default::default()
    };
    let mut visitor = new_node_visitor(Some(visit), None, hooks);
    visitor.visit_each_child(Some(root));
    if let Some(file) = file {
        let visit = visitor.visit.clone().unwrap();
        for &jsdoc in root.jsdoc(Some(file)) {
            visit(&mut visitor, jsdoc);
        }
    }
}

struct EncodeState {
    cx: EncodeContext,
    nodes: Vec<u8>,
    table: Vec<Option<P<Node>>>,
    node_index_map: Option<FxHashMap<P<Node>, u32>>,
    parent_index: u32,
    node_count: u32,
    prev_index: u32,
    error: Option<EncodeError>,
}

impl EncodeState {
    /// Go: `saveParentIndex := parentIndex; currentIndex := nodeCount; prevIndex = 0; parentIndex = currentIndex`.
    fn descend(&mut self) -> (u32, u32) {
        let saved = (self.parent_index, self.node_count);
        self.prev_index = 0;
        self.parent_index = self.node_count;
        saved
    }

    fn link_prev(&mut self) {
        if self.prev_index != 0 {
            let at = self.prev_index as usize * NODE_SIZE + NODE_OFFSET_NEXT;
            self.nodes[at..at + 4].copy_from_slice(&self.node_count.to_le_bytes());
        }
    }
}

impl Walker for EncodeState {
    fn enter_list(&mut self, list: &NodeList) -> (u32, u32) {
        self.node_count += 1;
        self.table.push(None);
        self.link_prev();
        let pos = self.cx.utf16(list.pos());
        let end = if list.nodes().last().is_some_and(|node| node.has_trailing_comma_marker()) {
            u32::MAX - 1
        } else {
            self.cx.utf16(list.end())
        };
        let values = [
            SYNTAX_KIND_NODE_LIST,
            pos,
            end,
            0,
            self.parent_index,
            list.nodes().len() as u32,
            list.has_trailing_comma() as u32,
        ];
        append_u32s(&mut self.nodes, &values);
        self.descend()
    }

    fn enter_node(&mut self, node: P<Node>) -> (u32, u32) {
        self.node_count += 1;
        self.table.push(Some(node));
        self.link_prev();
        let data = match get_node_data(node, &mut self.cx) {
            Ok(d) => d,
            Err(e) => {
                self.error.get_or_insert(e);
                0
            }
        };
        let values = [
            node.kind() as u32,
            self.cx.utf16(node.pos()),
            self.cx.utf16(node.end()),
            0,
            self.parent_index,
            data,
            node.flags().bits(),
        ];
        append_u32s(&mut self.nodes, &values);
        if let Some(map) = &mut self.node_index_map
            && let Some(slot) = map.get_mut(&node)
        {
            *slot = self.node_count;
        }
        self.descend()
    }

    fn leave(&mut self, (saved_parent, current): (u32, u32)) {
        self.prev_index = current;
        self.parent_index = saved_parent;
    }
}

fn get_node_data(node: P<Node>, cx: &mut EncodeContext) -> Result<u32, EncodeError> {
    let t = node_data_type(node);
    Ok(match t {
        NODE_DATA_TYPE_CHILDREN => {
            t | node_common_data(node)? | children_property_mask(node) as u32
        }
        NODE_DATA_TYPE_STRING => {
            let idx = record_node_strings(node, &mut cx.strs);
            if idx > NODE_DATA_STRING_INDEX_MASK {
                return Err(EncodeError::TooLarge("string table"));
            }
            t | node_common_data(node)? | idx
        }
        _ => {
            let off = record_extended_data(node, cx)?;
            if off > NODE_DATA_STRING_INDEX_MASK {
                return Err(EncodeError::TooLarge("extended data"));
            }
            t | node_common_data(node)? | off
        }
    })
}

fn encode_tree(
    root: P<Node>,
    source_file: Option<&'static SourceFile>,
) -> Result<(Vec<u8>, NodeIndexTable), EncodeError> {
    let is_source_file = root.kind() == Kind::SourceFile;
    let file_text: &'static str = if is_source_file { root.as_source_file().text() } else { "" };
    let position_map = PositionMap::compute(source_file.map_or("", |f| f.text()));
    let initial = source_file.map_or(0, |f| f.node_count.get());
    let mut node_index_map = None;
    if is_source_file {
        let sf = root.as_source_file();
        let emi = sf.external_module_indicator.get();
        let mut total = sf.imports.get().len() + sf.module_augmentations.get().len();
        if emi.is_some_and(|e| e != root) {
            total += 1;
        }
        if total > 0 {
            let mut map = FxHashMap::with_capacity_and_hasher(total, rustc_hash::FxBuildHasher);
            map.extend(
                sf.imports.get().iter().chain(sf.module_augmentations.get()).map(|&n| (n, 0)),
            );
            if let Some(e) = emi.filter(|&e| e != root) {
                map.insert(e, 0);
            }
            node_index_map = Some(map);
        }
    }
    let mut state = EncodeState {
        cx: EncodeContext {
            strs: StringTable::new(
                file_text,
                if is_source_file { source_file.map_or(0, |f| f.text_count.get()) } else { 0 },
            ),
            position_map,
            extended_data: Vec::new(),
            structured_data: Vec::new(),
        },
        nodes: Vec::with_capacity((initial + 2) * NODE_SIZE),
        table: Vec::with_capacity(initial + 2),
        node_index_map,
        parent_index: 0,
        node_count: 0,
        prev_index: 0,
        error: None,
    };
    state.table.push(None); // index 0 = nil sentinel
    append_u32s(&mut state.nodes, &[0, 0, 0, 0, 0, 0, 0]);

    state.node_count += 1;
    state.parent_index += 1;
    state.table.push(Some(root)); // index 1 = root node
    let sf_extended_data_offset = state.cx.extended_data.len();
    let data = get_node_data(root, &mut state.cx)?;
    let values = [
        root.kind() as u32,
        state.cx.utf16(root.pos()),
        state.cx.utf16(root.end()),
        0,
        0,
        data,
        root.flags().bits(),
    ];
    append_u32s(&mut state.nodes, &values);

    let state = Rc::new(RefCell::new(state));
    walk_children(root, source_file, Rc::clone(&state));
    let mut state = Rc::try_unwrap(state).ok().expect("visitor released").into_inner();
    if let Some(e) = state.error.take() {
        return Err(e);
    }

    let mut hash = 0u128;
    let mut parse_opts = 0u32;
    if is_source_file {
        let sf = root.as_source_file();
        hash = source_file.map_or(sf.hash.get(), |f| f.hash.get());
        parse_opts = encode_parse_options(sf.parse_options().external_module_indicator_options);
        let map = state.node_index_map.take().unwrap_or_default();
        let imports_offset =
            encode_node_index_array(sf.imports.get(), &map, &mut state.cx.structured_data);
        let augmentations_offset = encode_node_index_array(
            sf.module_augmentations.get(),
            &map,
            &mut state.cx.structured_data,
        );
        let ambient_offset =
            encode_string_array(sf.ambient_module_names.get(), &mut state.cx.structured_data);
        let ext = &mut state.cx.extended_data;
        ext[sf_extended_data_offset + 32..sf_extended_data_offset + 36]
            .copy_from_slice(&imports_offset.to_le_bytes());
        ext[sf_extended_data_offset + 36..sf_extended_data_offset + 40]
            .copy_from_slice(&augmentations_offset.to_le_bytes());
        ext[sf_extended_data_offset + 40..sf_extended_data_offset + 44]
            .copy_from_slice(&ambient_offset.to_le_bytes());
        let emi_index = match sf.external_module_indicator.get() {
            None => 0,
            Some(e) if e == root => 1,
            Some(e) => map.get(&e).copied().unwrap_or(0),
        };
        ext[sf_extended_data_offset + 44..sf_extended_data_offset + 48]
            .copy_from_slice(&emi_index.to_le_bytes());
    }

    let metadata = (PROTOCOL_VERSION as u32) << 24;
    let offset_string_offsets = HEADER_SIZE;
    let offset_string_data = HEADER_SIZE + state.cx.strs.offsets_len() * 4;
    let offset_extended_data = offset_string_data + state.cx.strs.string_length();
    let offset_structured_data = offset_extended_data + state.cx.extended_data.len();
    let offset_nodes = offset_structured_data + state.cx.structured_data.len();
    let total = offset_nodes + state.nodes.len();
    if total > u32::MAX as usize {
        return Err(EncodeError::TooLarge("source file"));
    }
    let (lo, hi) = (hash as u64, (hash >> 64) as u64);
    let header = [
        metadata,
        lo as u32,
        (lo >> 32) as u32,
        hi as u32,
        (hi >> 32) as u32,
        parse_opts,
        offset_string_offsets as u32,
        offset_string_data as u32,
        offset_extended_data as u32,
        offset_structured_data as u32,
        offset_nodes as u32,
        0,
        0, // source file ID
        0,
        0, // source file lease ID
        0, // binder data offset
    ];
    let mut out = Vec::with_capacity(total);
    append_u32s(&mut out, &header);
    state.cx.strs.encode_into(&mut out);
    out.extend_from_slice(&state.cx.extended_data);
    out.extend_from_slice(&state.cx.structured_data);
    out.extend_from_slice(&state.nodes);
    Ok((out, NodeIndexTable::new(state.table)))
}

/// Go `BuildNodeIndexTable`: the index table of `encode_source_file` without encoding. Indices are guaranteed to
/// match because both use the same walk.
pub fn build_node_index_table(file: &'static SourceFile) -> NodeIndexTable {
    struct Indexer {
        table: Vec<Option<P<Node>>>,
    }
    impl Walker for Indexer {
        fn enter_node(&mut self, node: P<Node>) -> (u32, u32) {
            self.table.push(Some(node));
            (0, 0)
        }
        fn enter_list(&mut self, _: &NodeList) -> (u32, u32) {
            self.table.push(None);
            (0, 0)
        }
        fn leave(&mut self, _: (u32, u32)) {}
    }
    let root = file.as_node();
    let mut table = Vec::with_capacity(file.node_count.get() + 2);
    table.push(None);
    table.push(Some(root));
    let state = Rc::new(RefCell::new(Indexer { table }));
    walk_children(root, Some(file), Rc::clone(&state));
    let Indexer { table } = Rc::try_unwrap(state).ok().expect("visitor released").into_inner();
    NodeIndexTable::new(table)
}

// ── hand-written extended data (Go recordExtendedData_*) ──────────────────────

#[expect(
    clippy::unnecessary_wraps,
    reason = "Extended-data encoder callbacks share a fallible signature with encoders that can reject node data."
)]
pub(crate) fn record_extended_data_source_file(
    node: P<Node>,
    cx: &mut EncodeContext,
) -> Result<(), EncodeError> {
    let sf = node.as_source_file();
    let text_index = cx.strs.add(sf.text(), Kind::SourceFile, node.pos(), node.end());
    // The API's project system does not content-map files yet (tsrs_project compilerhost.rs), so every file encoded
    // here has OriginalText() == Text(), no span map, no supplemental/canonical files, no content mapper / virtual
    // file name and no diagnostic directives. These are exactly the values the pinned encoder writes for a file
    // without content-mapper info; encoding content-mapper info is not implemented.
    let original_text_index = text_index;
    let file_name_index = cx.strs.add(sf.file_name(), Kind::Unknown, 0u32, 0u32);
    let path_index = cx.strs.add(&sf.path().0, Kind::Unknown, 0u32, 0u32);
    let pm = &cx.position_map;
    let referenced = encode_file_references(sf.referenced_files.get(), pm, &mut cx.structured_data);
    let type_refs =
        encode_file_references(sf.type_reference_directives.get(), pm, &mut cx.structured_data);
    let lib_refs =
        encode_file_references(sf.lib_reference_directives.get(), pm, &mut cx.structured_data);
    let span_map_offset = NO_STRUCTURED_DATA;
    let supplemental_offset = NO_STRUCTURED_DATA; // encodeStringArray(empty)
    let canonical_index = NO_STRUCTURED_DATA;
    let content_mapper_index = NO_STRUCTURED_DATA;
    let virtual_file_name_index = NO_STRUCTURED_DATA;
    let diagnostic_directives_offset = NO_STRUCTURED_DATA; // encodeDiagnosticDirectives(empty)
    append_u32s(
        &mut cx.extended_data,
        &[
            text_index,
            file_name_index,
            path_index,
            sf.language_variant() as u32,
            sf.script_kind() as u32,
            referenced,
            type_refs,
            lib_refs,
            NO_STRUCTURED_DATA, // imports (patched after the walk)
            NO_STRUCTURED_DATA, // moduleAugmentations (patched)
            NO_STRUCTURED_DATA, // ambientModuleNames (patched)
            0,                  // externalModuleIndicator (patched)
            original_text_index,
            span_map_offset,
            supplemental_offset,
            canonical_index,
            content_mapper_index,
            virtual_file_name_index,
            diagnostic_directives_offset,
        ],
    );
    Ok(())
}

fn record_template_like(
    node: P<Node>,
    text: &str,
    raw_text: &str,
    flags: TokenFlags,
    cx: &mut EncodeContext,
) {
    let text_index = cx.strs.add(text, node.kind(), node.pos(), node.end());
    let raw_index = cx.strs.add(raw_text, node.kind(), node.pos(), node.end());
    append_u32s(&mut cx.extended_data, &[text_index, raw_index, flags.bits()]);
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "Extended-data encoder callbacks share a fallible signature with encoders that can reject node data."
)]
pub(crate) fn record_extended_data_template_head(
    node: P<Node>,
    cx: &mut EncodeContext,
) -> Result<(), EncodeError> {
    let n = node.as_template_head();
    record_template_like(node, n.text(), n.raw_text(), n.template_flags(), cx);
    Ok(())
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "Extended-data encoder callbacks share a fallible signature with encoders that can reject node data."
)]
pub(crate) fn record_extended_data_template_middle(
    node: P<Node>,
    cx: &mut EncodeContext,
) -> Result<(), EncodeError> {
    let n = node.as_template_middle();
    record_template_like(node, n.text(), n.raw_text(), n.template_flags(), cx);
    Ok(())
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "Extended-data encoder callbacks share a fallible signature with encoders that can reject node data."
)]
pub(crate) fn record_extended_data_template_tail(
    node: P<Node>,
    cx: &mut EncodeContext,
) -> Result<(), EncodeError> {
    let n = node.as_template_tail();
    record_template_like(node, n.text(), n.raw_text(), n.template_flags(), cx);
    Ok(())
}

fn record_literal(node: P<Node>, text: &str, flags: TokenFlags, cx: &mut EncodeContext) {
    let text_index = cx.strs.add(text, node.kind(), node.pos(), node.end());
    append_u32s(&mut cx.extended_data, &[text_index, flags.bits()]);
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "Extended-data encoder callbacks share a fallible signature with encoders that can reject node data."
)]
pub(crate) fn record_extended_data_string_literal(
    node: P<Node>,
    cx: &mut EncodeContext,
) -> Result<(), EncodeError> {
    let n = node.as_string_literal();
    record_literal(node, n.text(), n.token_flags(), cx);
    Ok(())
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "Extended-data encoder callbacks share a fallible signature with encoders that can reject node data."
)]
pub(crate) fn record_extended_data_numeric_literal(
    node: P<Node>,
    cx: &mut EncodeContext,
) -> Result<(), EncodeError> {
    let n = node.as_numeric_literal();
    record_literal(node, n.text(), n.token_flags(), cx);
    Ok(())
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "Extended-data encoder callbacks share a fallible signature with encoders that can reject node data."
)]
pub(crate) fn record_extended_data_big_int_literal(
    node: P<Node>,
    cx: &mut EncodeContext,
) -> Result<(), EncodeError> {
    let n = node.as_big_int_literal();
    record_literal(node, n.text(), n.token_flags(), cx);
    Ok(())
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "Extended-data encoder callbacks share a fallible signature with encoders that can reject node data."
)]
pub(crate) fn record_extended_data_regular_expression_literal(
    node: P<Node>,
    cx: &mut EncodeContext,
) -> Result<(), EncodeError> {
    let n = node.as_regular_expression_literal();
    record_literal(node, n.text(), n.token_flags(), cx);
    Ok(())
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "Extended-data encoder callbacks share a fallible signature with encoders that can reject node data."
)]
pub(crate) fn record_extended_data_no_substitution_template_literal(
    node: P<Node>,
    cx: &mut EncodeContext,
) -> Result<(), EncodeError> {
    let n = node.as_no_substitution_template_literal();
    record_literal(node, n.text(), n.template_flags(), cx);
    Ok(())
}

/// Go `getNodeCommonData_SyntheticExpression` panics: the node is checker-internal and never encoded.
pub(crate) fn node_common_data_synthetic_expression(node: P<Node>) -> Result<u32, EncodeError> {
    Err(EncodeError::Unencodable(node.kind()))
}

// ── structured data (msgpack) ─────────────────────────────────────────────────

fn encode_file_references(refs: &[P<FileReference>], pm: &PositionMap, buf: &mut Vec<u8>) -> u32 {
    if refs.is_empty() {
        return NO_STRUCTURED_DATA;
    }
    let offset = buf.len() as u32;
    msgpack::write_array_header(buf, refs.len());
    for r in refs {
        msgpack::write_array_header(buf, 5);
        msgpack::write_uint(
            buf,
            pm.utf8_to_utf16(i64::from(
                r.text_range.pos().expect_source("file references have source positions"),
            )) as u32,
        );
        msgpack::write_uint(
            buf,
            pm.utf8_to_utf16(i64::from(
                r.text_range.end().expect_source("file references have source positions"),
            )) as u32,
        );
        msgpack::write_string(buf, &r.file_name);
        msgpack::write_uint(buf, r.resolution_mode.value() as u32);
        msgpack::write_bool(buf, r.preserve);
    }
    offset
}

fn encode_node_index_array(
    nodes: &[P<Node>],
    map: &FxHashMap<P<Node>, u32>,
    buf: &mut Vec<u8>,
) -> u32 {
    if nodes.is_empty() {
        return NO_STRUCTURED_DATA;
    }
    let offset = buf.len() as u32;
    msgpack::write_array_header(buf, nodes.len());
    for n in nodes {
        msgpack::write_uint(buf, map.get(n).copied().unwrap_or(0));
    }
    offset
}

fn encode_string_array(strs: &[&str], buf: &mut Vec<u8>) -> u32 {
    if strs.is_empty() {
        return NO_STRUCTURED_DATA;
    }
    let offset = buf.len() as u32;
    msgpack::write_array_header(buf, strs.len());
    for s in strs {
        msgpack::write_string(buf, s);
    }
    offset
}
