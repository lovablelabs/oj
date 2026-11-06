// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Where TanStack Start keeps its routes and generated route tree. Start
//! resolves both from `tanstackStart({ srcDirectory, router })`
//! (start-plugin-core's `parseStartConfig`: `<srcDirectory>/<routesDirectory>`
//! and `<srcDirectory>/<generatedRouteTree>`, default `src/routes` and
//! `src/routeTree.gen.ts`) and passes them to the router generator inline, so
//! they win over tsr.config.json. Extraction records the paths that generator
//! ran with when the app's config resolved (`vite_start_router`).

use std::path::{Path, PathBuf};

use crate::OjConfig;

/// The routes directory and generated route tree Start's router generator
/// ran with, absolute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRouterPaths {
    pub routes_directory: PathBuf,
    pub generated_route_tree: PathBuf,
}

/// Start's routes directory for the app at `root`.
pub fn start_routes_dir(config: &OjConfig, root: &Path) -> PathBuf {
    match &config.vite_start_router {
        Some(paths) => root.join(&paths.routes_directory),
        None => root.join("src").join("routes"),
    }
}

/// Start's generated route tree for the app at `root`.
pub fn start_route_tree(config: &OjConfig, root: &Path) -> PathBuf {
    match &config.vite_start_router {
        Some(paths) => root.join(&paths.generated_route_tree),
        None => root.join("src").join("routeTree.gen.ts"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_defaults_without_a_recorded_generator_run() {
        let root = Path::new("/app");
        let config = OjConfig::default();
        assert_eq!(start_routes_dir(&config, root), root.join("src/routes"));
        assert_eq!(
            start_route_tree(&config, root),
            root.join("src/routeTree.gen.ts")
        );
    }

    #[test]
    fn the_recorded_paths_win() {
        let root = Path::new("/app");
        let config = OjConfig {
            vite_start_router: Some(StartRouterPaths {
                routes_directory: "/app/app/pages".into(),
                generated_route_tree: "/app/app/tree.gen.ts".into(),
            }),
            ..Default::default()
        };
        assert_eq!(start_routes_dir(&config, root), root.join("app/pages"));
        assert_eq!(
            start_route_tree(&config, root),
            root.join("app/tree.gen.ts")
        );
    }
}
