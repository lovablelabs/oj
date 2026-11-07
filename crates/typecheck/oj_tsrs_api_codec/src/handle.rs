//! Node handle strings (pinned session.go `nodeHandleFrom` / `resolveNodeHandle`): `"<index>.<kind>.<path>"`, where `index` is the `NodeIndexTable` index of the node in its source file.
//!
//! Resolving the path to a source file and the index to a node is the session's job (it owns the snapshot
//! that keeps the file alive).

use tsrs_ast::{Node, SourceFile};
use tsrs_core::P;

use crate::encoder::NodeIndexTable;

/// Go `nodeHandleFrom` given the node's file and that file's index table.
pub fn node_handle(table: &NodeIndexTable, file: &SourceFile, node: P<Node>) -> String {
    format!("{}.{}.{}", table.get_index(node), node.kind() as u32, &*file.path().0)
}

/// Preserves the components and validation rules of Go node handles.
///
/// The parts of a node handle as `resolveNodeHandle` parses them: the index must be a uint32, the kind is
/// informational only (Go ignores it; it is returned unparsed), the path is everything after the second dot.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedNodeHandle<'a> {
    pub index: u32,
    pub kind: &'a str,
    pub path: &'a str,
}

pub fn parse_node_handle(handle: &str) -> Option<ParsedNodeHandle<'_>> {
    let (index, rest) = handle.split_once('.')?;
    let (kind, path) = rest.split_once('.')?;
    // strconv.ParseUint(s, 10, 32): decimal digits only (no sign), at most u32::MAX.
    if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(ParsedNodeHandle { index: index.parse().ok()?, kind, path })
}

/// Go `resolveNodeHandle` after the file lookup: the node at the handle's index, if any.
pub fn resolve_node_index(table: &NodeIndexTable, index: u32) -> Option<P<Node>> {
    table.node(index)
}
