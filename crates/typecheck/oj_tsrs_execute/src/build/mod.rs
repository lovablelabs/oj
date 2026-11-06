// Port of Go's execute/build package (`tsc -b`), without watch mode. Go keeps it in its own package; here it is a
// module of tsrs_execute because it depends on the `tsc` module of this crate (Go's execute/tsc).

pub mod buildtask;
mod compilerhost;
mod host;
pub mod orchestrator;
mod parsecache;
pub mod uptodatestatus;

pub use orchestrator::{
    Options, Orchestrator, OrchestratorResult, free_api_orchestrator, new_orchestrator,
};
