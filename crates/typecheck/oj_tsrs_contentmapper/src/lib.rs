// Go `internal/contentmapper`, the host half (notes/contentmappers.md): host.go (the errors, the transform result
// types, and the Project and Host interfaces), hostimpl.go (the process host: protocol types, project leases,
// response decoding and position normalization), transform.go (parsing a transform result into source files), and
// process.rs, the production Spawner (Go cmd/tsc/sys.go spawnProcess). The definitions half of the package (Mapper,
// Definition, Manifest, IsSupportedVirtualExtension, OptionPathSegment) is tsrs_tsoptions::contentmappers; it is
// re-exported here so that `contentmapper::Mapper` reads as in Go.

pub mod host;
pub mod hostimpl;
pub mod process;
pub mod transform;

pub use host::*;
pub use hostimpl::*;
pub use process::*;
pub use transform::*;
pub use tsrs_tsoptions::{
    Definition, Manifest, Mapper, OptionPathSegment, is_supported_virtual_extension,
};
