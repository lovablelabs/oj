// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

pub mod assets;
pub mod bundle;
pub mod cjs;
mod compile;
mod env;
pub mod glob;
mod hot;
pub mod html;
pub mod interop;
pub mod json;
mod rewrite;
mod scan;
mod sourcemap;
pub mod ssr;
pub mod tsconfig;

pub(crate) use compile::detect_refresh_registrations;
pub use compile::{
    compile, compile_module, compile_module_with_maps, CompileError, CompileOptions, CompileOutput,
    HotAccept, ImportRewriter, JsxConfig, COMPILE_STACK_SIZE,
};
pub(crate) use env::defines_for;
pub use env::ImportMetaEnv;
pub(crate) use rewrite::rewrite_module_specifiers_pub;
pub use scan::{exports, imports, rewrite_specifiers, specifier_edits, SpecifierEdits};
pub(crate) use scan::{scan, F_IMPORT_META_GLOB, F_IMPORT_PAREN};
pub use sourcemap::map_json_to_data_url;
