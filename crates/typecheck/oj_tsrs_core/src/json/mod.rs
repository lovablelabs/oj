#[expect(
    clippy::module_inception,
    reason = "The module path preserves the upstream Go package and file layout."
)]
pub mod json;
pub mod map;
pub mod plain;

pub use json::*;
pub(crate) use map::Entry;
pub use map::{JsonMap, Key};
pub use plain::plain_ascii_len;
