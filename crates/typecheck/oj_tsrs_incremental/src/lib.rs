// Port of Go's execute/incremental package: incremental programs, tsbuildinfo read/write and affected-file
// tracking.

mod affectedfileshandler;
pub mod buildinfo;
mod buildinfotosnapshot;
pub mod emit;
mod emitfileshandler;
pub mod host;
mod incremental;
pub mod program;
mod programtosnapshot;
mod referencemap;
pub mod snapshot;
mod snapshottobuildinfo;

pub use buildinfo::*;
pub use host::{Host, create_host, get_m_time};
pub use incremental::{
    BuildInfoReader, new_build_info_reader, new_build_info_reader_with_canonical_diagnostics,
    read_build_info_program, register_build_info_read_phases,
};
pub use program::{
    Program, SemanticDiagnosticsState, SignatureUpdateKind, TestingData, new_program,
};
pub use snapshot::{FileEmitKind, FileInfo, compute_hash, get_file_emit_kind};
