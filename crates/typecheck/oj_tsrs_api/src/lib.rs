// Port of microsoft/TypeScript tsc/internal/api (session, proto, callbackfs, requestfilesystem,
// module_resolution) at the commit pinned in the workspace Cargo.toml. See docs/NODE_API.md for the
// integration contract, lane ownership and the per-method coverage matrix.
//
// Lane ownership (Node API effort):
// - core (this crate, except `checker`): session lifecycle, dispatch, snapshots/programs, config,
//   diagnostics, emit/transpile/build, module resolution, request/callback filesystems.
// - checker: `src/checker.rs` and `src/checker/**` (symbol/type/signature/checker/LS query methods).
// - codec: crates/tsrs_api_codec (AST binary encoder/decoder, node index tables).
// - runtime: crates/tsrs_api_transport (MessagePack tuple protocol, JSON-RPC, sync/async conns).

pub mod adapter;
pub(crate) mod batch;
pub mod build;
pub mod config;
pub mod diagnostics;
pub mod handler;
pub mod methods;
pub mod module_resolution;
mod oj_incremental;
pub(crate) mod paramfields;
pub mod predecode;
pub mod printing;
pub(crate) mod program;
pub mod raw_options;
pub mod requestfs;
mod resolution;
pub mod session;
pub mod snapshots;
pub mod sourcefiles;
pub mod transpile;
pub mod wire;

pub mod checker;

pub use handler::{ApiError, ApiResult, ClientConn, ErrorKind, Handler, Response};
pub use session::{Session, SessionOptions};
