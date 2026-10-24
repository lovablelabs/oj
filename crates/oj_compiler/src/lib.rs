// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

pub mod assets;
pub mod bundle;
pub mod cjs;
mod compile;
mod env;
pub mod env_fns;
pub mod glob;
mod hot;
pub mod html;
pub mod interop;
pub mod json;
pub mod meta_glob;
mod rewrite;
mod scan;
pub mod server_fns;
mod sourcemap;
pub mod ssr;
pub mod tsconfig;

pub(crate) use compile::detect_refresh_registrations;
pub use compile::{
    compile, compile_module, compile_module_with_maps, transform_for_bundle, CompileError,
    CompileOptions, CompileOutput, HotAccept, ImportRewriter, JsxConfig, COMPILE_STACK_SIZE,
};
pub(crate) use env::defines_for;
pub use env::ImportMetaEnv;
pub(crate) use rewrite::rewrite_module_specifiers_pub;
pub use scan::{exports, imports, rewrite_specifiers, specifier_edits, SpecifierEdits};
pub(crate) use scan::{scan, F_IMPORT_META_GLOB, F_IMPORT_PAREN};
pub use sourcemap::map_json_to_data_url;

/// A string literal's value as text for analysis (specifiers, export names,
/// keys): oxc 0.155 stores it as WTF-8, and a value with a lone surrogate,
/// never a valid specifier or name here, reads as U+FFFD.
pub fn lit_str(value: oxc_str::JSStr<'_>) -> &'_ str {
    value.as_str().unwrap_or("\u{FFFD}")
}

#[cfg(test)]
mod lit_str_tests {
    #[test]
    fn a_lone_surrogate_reads_as_the_replacement_character() {
        let allocator = oxc_allocator::Allocator::default();
        let source = "import \"./a.js\"; import \"\\uD800\";";
        let parsed =
            oxc_parser::Parser::new(&allocator, source, oxc_span::SourceType::mjs()).parse();
        let sources: Vec<&str> = parsed
            .program
            .body
            .iter()
            .filter_map(|s| match s {
                oxc_ast::ast::Statement::ImportDeclaration(d) => {
                    Some(super::lit_str(d.source.value))
                }
                _ => None,
            })
            .collect();
        assert_eq!(sources, ["./a.js", "\u{FFFD}"]);
    }
}
