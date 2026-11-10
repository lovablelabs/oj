import { createStartHandler, defaultStreamHandler } from "@tanstack/react-start/server";

// oj's stand-in for `@tanstack/react-start/server-entry` (the package's
// dist/default-entry/esm/server.js): it carries the same exports, so an
// app's own entry written as the package documents it, `import handler,
// { createServerEntry } from "@tanstack/react-start/server-entry"`, composes
// with oj's handler as it does with Start's under Vite.
export type ServerEntry = {
  fetch: (request: Request, env?: unknown, ctx?: unknown) => Promise<Response> | Response;
};

// As the package's `createServerEntry`: the entry's `fetch`, awaited, with
// every argument passed on.
export function createServerEntry(entry: ServerEntry): ServerEntry {
  return {
    async fetch(...args) {
      return await entry.fetch(...args);
    },
  };
}

const handler = createStartHandler(defaultStreamHandler);
export default createServerEntry({
  async fetch(request: Request, env?: unknown, ctx?: unknown): Promise<Response> {
    if (env) {
      (globalThis as Record<string, unknown>).__OJ_CF_ENV = env;
      (globalThis as Record<string, unknown>).__OJ_CF_CTX = ctx;
    }
    return await handler(request);
  },
});
