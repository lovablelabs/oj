// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// What the plugin host's engine runs before `plugin-host.mjs`, as calls. An
// eval waits for the whole event loop to go idle, so a timer the app's vite
// leaves running at import would hold every later eval, and the host's boot
// with it, for as long as the timer lives.

import { esmEntry } from "./vite-config-node.mjs";
import { createRequire } from "node:module";

// The host's identity, read by plugin-host.mjs when it is imported.
export function seed(boot) {
  globalThis.__ojPluginHost = boot;
}

// The specifier the host's loadViteConfig imports, so the module it finds is
// this one, already evaluated. A failure is the host's to report when it
// imports for real.
export async function warmVite(anchor) {
  try {
    await import(esmEntry(createRequire(anchor).resolve("vite"), "vite"));
  } catch {}
}
