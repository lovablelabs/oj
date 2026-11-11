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
type RequestOptions = Parameters<typeof handler>[1];

// A Worker runtime calls `fetch(request, env, ctx)`: its third argument is the
// execution context.
function isExecutionContext(ctx: unknown): boolean {
  return typeof (ctx as { waitUntil?: unknown } | null | undefined)?.waitUntil === "function";
}

// Start's handler is `(request, opts?)`, and an app's entry hands it the
// request's options, `handler.fetch(request, { context })`: they reach the
// handler as they do through the package's entry. Vite's dev and preview
// middlewares call `fetch(request)` alone, and a Worker runtime calls
// `fetch(request, env, ctx)`: only then is the second argument the Worker's
// bindings, stashed for `getCloudflareContext()`.
export default createServerEntry({
  async fetch(request: Request, second?: unknown, third?: unknown): Promise<Response> {
    if (isExecutionContext(third)) {
      (globalThis as Record<string, unknown>).__OJ_CF_ENV = second;
      (globalThis as Record<string, unknown>).__OJ_CF_CTX = third;
      return await handler(request);
    }
    return await handler(request, second as RequestOptions);
  },
});
