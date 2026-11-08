// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The TanStack Start flavor under build (`@tanstack/react-start` or
// `@tanstack/solid-start`): oj sets OJ_START_PKG (and friends) for every Start
// script; unset (unit tests, bare `node <script>`) keeps the historical React
// defaults. Read lazily — `run(env)` applies the env pairs after import, so
// module-level reads would only see the ambient env.
export function startPkg() {
  return process.env.OJ_START_PKG || "@tanstack/react-start";
}

export function routerTarget() {
  return process.env.OJ_ROUTER_TARGET || "react";
}

export function clientEntryFile() {
  return process.env.OJ_CLIENT_ENTRY || "client-entry.tsx";
}

export function serverEntryFile() {
  return process.env.OJ_SERVER_ENTRY_FILE || "server-entry.tsx";
}

// TanStack Start's default Worker entry (`main: "<pkg>/server-entry"`),
// which oj replaces with its own server entry the way it does for the Node
// build.
export function startDefaultEntry() {
  const pkg = startPkg().replaceAll("/", "[\\\\/]");
  return new RegExp(`[\\\\/]${pkg}[\\\\/]dist[\\\\/]default-entry[\\\\/]`);
}
