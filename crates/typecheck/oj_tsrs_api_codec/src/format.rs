//! Wire constants of the binary source file format (pinned `tsc/internal/api/encoder/encoder.go`).
//!
//! The layout is documented there in full ("Source File Binary Format"); the client decoder is
//! `packages/typescript/src/api/node/node.infrastructure.ts` + `protocol.generated.ts`.

// Byte offsets of the fields of one node record.
pub const NODE_OFFSET_KIND: usize = 0;
pub const NODE_OFFSET_POS: usize = 4;
pub const NODE_OFFSET_END: usize = 8;
pub(crate) const NODE_OFFSET_NEXT: usize = 12;
pub const NODE_OFFSET_PARENT: usize = 16;
pub const NODE_OFFSET_DATA: usize = 20;
pub const NODE_OFFSET_FLAGS: usize = 24;
/// Bytes per node record.
pub const NODE_SIZE: usize = 28;

pub(crate) const NODE_DATA_TYPE_CHILDREN: u32 = 0 << 30;
pub const NODE_DATA_TYPE_STRING: u32 = 1 << 30;
pub const NODE_DATA_TYPE_EXTENDED_DATA: u32 = 2 << 30;

pub const NODE_DATA_TYPE_MASK: u32 = 0xc0_00_00_00;
pub(crate) const NODE_DATA_CHILD_MASK: u32 = 0x00_00_00_ff;
pub const NODE_DATA_STRING_INDEX_MASK: u32 = 0x00_ff_ff_ff;

/// `kind` of a `NodeList` record.
pub const SYNTAX_KIND_NODE_LIST: u32 = u32::MAX;

// Byte offsets of the header fields.
pub const HEADER_OFFSET_METADATA: usize = 0;
pub const HEADER_OFFSET_HASH_LO0: usize = 4;
pub const HEADER_OFFSET_HASH_LO1: usize = 8;
pub const HEADER_OFFSET_HASH_HI0: usize = 12;
pub const HEADER_OFFSET_HASH_HI1: usize = 16;
pub const HEADER_OFFSET_PARSE_OPTIONS: usize = 20;
pub const HEADER_OFFSET_STRING_OFFSETS: usize = 24;
pub const HEADER_OFFSET_STRING_DATA: usize = 28;
pub const HEADER_OFFSET_EXTENDED_DATA: usize = 32;
pub const HEADER_OFFSET_STRUCTURED_DATA: usize = 36;
pub const HEADER_OFFSET_NODES: usize = 40;
pub(crate) const HEADER_OFFSET_SOURCE_FILE_ID: usize = 44;
pub(crate) const HEADER_OFFSET_SOURCE_FILE_LEASE: usize = 52;
pub const HEADER_OFFSET_BINDER_DATA: usize = 60;
pub(crate) const HEADER_SIZE: usize = 64;

/// Pinned `encoder.ProtocolVersion`, stored in the high byte of the metadata word.
pub const PROTOCOL_VERSION: u8 = 9;

/// "No data" marker for optional structured data / string indices in the `SourceFile` extended data.
pub(crate) const NO_STRUCTURED_DATA: u32 = 0xFFFF_FFFF;

/// Byte size of the `SourceFile` extended data record (19 uint32 fields).
pub const SOURCE_FILE_EXTENDED_DATA_SIZE: usize = 76;
