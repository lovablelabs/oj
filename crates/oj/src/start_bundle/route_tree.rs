// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The client's route tree. Start's `tanstack-start:route-tree-client-plugin`
//! (`enforce: "pre"`, client environment only) loads the generated tree's id
//! with the tree rebuilt from the generator's crawl, server-only subtrees
//! pruned (`pruneServerOnlySubtrees`): a route whose only option is `server`
//! never reaches the browser. generate.mjs writes that tree beside the
//! generated one; this plugin loads it in its place.

use std::borrow::Cow;
use std::path::PathBuf;

use rolldown_common::ModuleType;
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, Plugin, SharedLoadPluginContext,
};

#[derive(Debug)]
pub(crate) struct ClientRouteTree {
    /// The generated tree the app imports (`generatedRouteTree`).
    pub tree: PathBuf,
    /// The client's tree generate.mjs wrote.
    pub client: PathBuf,
}

impl ClientRouteTree {
    pub fn new(start: &super::StartConfig) -> Self {
        Self {
            tree: start.route_tree(),
            client: start.cache.join(super::CLIENT_ROUTE_TREE),
        }
    }
}

impl Plugin for ClientRouteTree {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:start-client-route-tree")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::Load
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        let file = args.id.split(['?', '#']).next().unwrap_or(args.id);
        if std::path::Path::new(file) != self.tree {
            return Ok(None);
        }
        // No client tree (a generator that could not crawl): the generated
        // tree serves, as before.
        let Ok(code) = std::fs::read_to_string(&self.client) else {
            return Ok(None);
        };
        Ok(Some(HookLoadOutput {
            code: code.into(),
            module_type: Some(ModuleType::Js),
            ..Default::default()
        }))
    }
}
