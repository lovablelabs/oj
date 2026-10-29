// Port of Go's `execute/tsc` package (compile, emit, diagnostics reporting, statistics).

mod compile;
mod diagnostics;
mod emit;
mod extendedconfigcache;
mod statistics;

pub use compile::*;
pub use diagnostics::*;
pub(crate) use emit::*;
pub use extendedconfigcache::*;
pub use statistics::*;
// The stream `System::spawn` returns (Go `io.ReadWriteCloser`).
pub use tsrs_ipc::ReadWriteCloser;
