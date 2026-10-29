#[expect(
    clippy::module_inception,
    reason = "The module path preserves the upstream Go package and file layout."
)]
pub mod vfstest;

pub use self::vfstest::*;
