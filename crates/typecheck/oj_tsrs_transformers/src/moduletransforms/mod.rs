//! Go package `transformers/moduletransforms`.

use crate::*;

mod commonjsmodule;
mod commonjsmodule_2;
mod commonjsmodule_3;
mod commonjsmodule_4;
mod esmodule;
mod externalmoduleinfo;
mod impliedmodule;
mod utilities;

pub use commonjsmodule::*;
pub use esmodule::*;
pub(crate) use externalmoduleinfo::*;
pub use impliedmodule::*;
pub(crate) use utilities::{
    create_empty_imports, get_external_module_name_literal,
    is_declaration_name_of_enum_or_namespace, is_file_level_reserved_generated_identifier,
    rewrite_module_specifier,
};
