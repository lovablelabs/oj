import { createRouter } from "@tanstack/solid-router";

import { routeTree } from "./routeTree.gen";

// oj aliases `#tanstack-router-entry` to this module and TanStack Start's
// handler imports getRouter() from it (same convention as the React fixture).
// The tree is the TanStack-generated file-route tree, shared by the reference
// Vite runner and oj.
export function getRouter() {
  return createRouter({
    routeTree,
    defaultPreload: "intent",
    scrollRestoration: true,
  });
}
