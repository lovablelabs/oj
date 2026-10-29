// Go package internal/jsonrpc.

pub mod baseproto;
#[expect(
    clippy::module_inception,
    reason = "The module path preserves the upstream Go package and file layout."
)]
pub mod jsonrpc;

pub use self::baseproto::*;
pub use self::jsonrpc::*;
