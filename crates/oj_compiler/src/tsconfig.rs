// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Nearest-tsconfig discovery for the transform, mirroring Vite's vite:oxc
//! slot: rolldown passes tsconfig discovery `Auto` to oxc, which uses
//! oxc_resolver's `find_tsconfig` (ancestor search, `extends` merged,
//! `files`/`include`/`exclude` honored, `None` inside node_modules).
//! Only the class-field semantics are consumed here, derived like rolldown:
//! an explicit `useDefineForClassFields` wins, else `target` decides (`esnext`
//! or ES2022+ mean define, anything else including no target means assign),
//! and no tsconfig means the transform default (define). rolldown also maps
//! `verbatimModuleSyntax`, `experimentalDecorators` and `jsx*`; not yet here.

use std::path::Path;
use std::sync::OnceLock;

use oxc_resolver::{ResolveOptions, Resolver, TsconfigDiscovery};

fn resolver() -> &'static Resolver {
    static RESOLVER: OnceLock<Resolver> = OnceLock::new();
    RESOLVER.get_or_init(|| {
        Resolver::new(ResolveOptions {
            tsconfig: Some(TsconfigDiscovery::Auto),
            ..ResolveOptions::default()
        })
    })
}

/// rolldown's `is_use_define_for_class_fields`: whether an ES `target` implies
/// `useDefineForClassFields: true` when the option itself is not set.
fn target_implies_define(target: Option<&str>) -> bool {
    let Some(target) = target else { return false };
    if target.len() < 3 || !target[..2].eq_ignore_ascii_case("es") {
        return false;
    }
    let rest = &target[2..];
    if rest.eq_ignore_ascii_case("next") {
        return true;
    }
    rest.parse::<usize>().is_ok_and(|x| x > 2021)
}

/// True when the nearest owning tsconfig gives this TypeScript file [[Set]]
/// assignment semantics for class fields (`this.foo = 1` in the constructor
/// instead of a field declaration). False for non-TS files, files no tsconfig
/// owns, and configs whose effective `useDefineForClassFields` is true.
pub fn class_field_set_semantics(path: &Path) -> bool {
    let is_ts = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("ts" | "tsx" | "mts" | "cts")
    );
    if !is_ts {
        return false;
    }
    // A malformed tsconfig is swallowed (no semantics change) rather than
    // failing the transform; surfacing it like Vite is tracked separately.
    let Ok(Some(tsconfig)) = resolver().find_tsconfig(path) else {
        return false;
    };
    let opts = &tsconfig.compiler_options;
    let define = opts
        .use_define_for_class_fields
        .unwrap_or_else(|| target_implies_define(opts.target.as_deref()));
    !define
}

/// Drops every cached tsconfig lookup: the compiler half of Vite's
/// `reloadOnTsconfigChange`, called by the dev server when a tsconfig changes.
pub fn clear_cache() {
    resolver().clear_cache();
}
