// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! oj's stylesheet pipeline: lightningcss for CSS (modules, lowering, url
//! rebasing), grass for Sass, and postcss-import style `@import` inlining,
//! each resolving specifiers the way Vite does.

mod compile;
mod imports;
mod modules;
mod path;
mod rebase;
mod resolve;
mod sass;
mod targets;

pub use compile::{
    compile_css, compile_css_dev, compile_css_rebased, compile_css_rebased_with_map,
    compile_css_with, CssOutput,
};
pub use imports::{
    inline_imports, inline_imports_collecting, inline_imports_with, resolve_css_import,
    resolve_css_import_with,
};
pub use modules::{css_modules_esm, is_css_module};
pub use path::node_modules_load_paths;
pub use resolve::{CssModulesOptions, CssResolve, CssResolveConfig};
pub use sass::{
    compile_sass, compile_sass_collecting, compile_sass_opts, compile_sass_with, is_sass,
    SassOptions,
};
pub use targets::browser_targets;
