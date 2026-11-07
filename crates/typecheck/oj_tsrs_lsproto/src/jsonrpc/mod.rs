// Go package internal/jsonrpc.

#[expect(
    clippy::redundant_pub_crate,
    reason = "The framing constructors are re-exported only for the crate's LSP framing layer."
)]
mod baseproto;
#[expect(
    clippy::module_inception,
    reason = "The module path preserves the upstream Go package and file layout."
)]
mod jsonrpc;

pub use self::baseproto::*;
pub use self::jsonrpc::*;
