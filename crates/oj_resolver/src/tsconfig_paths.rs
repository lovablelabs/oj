// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! How the app's Vite applies tsconfig `paths`. vite-tsconfig-paths answers
//! for itself: oj's plugin host runs it ahead of oj's resolver wherever Vite
//! runs it (dev, SSR, the dep scan, the Start client, builds and plugins'
//! `this.resolve`), with its own options, `skip` and `importerFilter`.
//! Vite's own resolver (`resolve.tsconfigPaths`) is oxc's, which oj's
//! resolver runs per importer as Vite does.

/// How the app's Vite applies tsconfig `paths`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TsconfigPaths {
    /// vite-tsconfig-paths runs: in the plugin host, ahead of oj's
    /// resolver, with its own options, so oj applies no `paths` itself (what
    /// reaches oj the plugin passed on to vite:resolve).
    Plugin,
    /// vite-tsconfig-paths and `resolve.tsconfigPaths: true`: the plugin
    /// first, Vite's own resolver for whatever it passes on.
    PluginNative,
    /// `resolve.tsconfigPaths: true`: Vite 8's own resolver (oxc, the
    /// tsconfig that owns the importer), an exact key first, a declaration
    /// file served like any file.
    Native,
    /// Neither (or the plugins are unknown): Vite's dev server applies no
    /// `paths`. `vite build` lets rolldown's own resolver apply them to what
    /// its resolve plugin leaves (`TsconfigAnswer::Bundler` for a build).
    #[default]
    Neither,
    /// No `paths` at all (Vite's module runner resolving an external).
    Off,
}
