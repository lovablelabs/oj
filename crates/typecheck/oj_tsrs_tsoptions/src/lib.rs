pub mod commandlineoption;
pub mod commandlineparser;
pub mod contentmappers;
#[expect(
    clippy::redundant_pub_crate,
    reason = "Build-option tables are shared with the parser within this crate."
)]
mod declsbuild;
pub mod declscompiler;
mod declstypeacquisition;
pub mod diagnostics;
pub mod enummaps;
pub mod errors;
pub mod namemap;
pub mod parsedbuildcommandline;
pub mod parsedcommandline;
pub mod parsedoptions;
pub mod parsinghelpers;
pub mod tsconfigparsing;
pub mod wildcarddirectories;

pub mod gojson;
pub mod outputpaths;

pub mod tsoptionstest;

pub use commandlineoption::*;
pub use commandlineparser::*;
pub use contentmappers::*;
pub use declscompiler::*;

pub use enummaps::*;
pub use errors::*;
pub use namemap::*;
pub use parsedbuildcommandline::*;
pub use parsedcommandline::*;
pub use parsedoptions::*;
pub use parsinghelpers::*;
pub use tsconfigparsing::*;
