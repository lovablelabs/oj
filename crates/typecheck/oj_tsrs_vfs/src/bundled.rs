#[expect(
    clippy::module_inception,
    reason = "The module path preserves the upstream Go package and file layout."
)]
mod bundled;
// The test runners use this explicitly, including when training noembed release builds.
// Its library data is unreferenced and removed by the linker in a noembed CLI.
pub mod embed;
mod embed_generated;
mod libs_generated;
#[cfg(feature = "noembed")]
pub mod noembed;

pub use self::bundled::*;
#[cfg(not(feature = "noembed"))]
use self::embed as implementation;
pub use self::implementation::is_bundled;
pub use self::libs_generated::*;
#[cfg(feature = "noembed")]
use self::noembed as implementation;
