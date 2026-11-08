import { createStartHandler, defaultStreamHandler } from "@tanstack/solid-start/server";
import * as serverRuntime from "@tanstack/solid-start/server";
const handler = createStartHandler(defaultStreamHandler);
export default {
  async fetch(request: Request, env?: unknown, ctx?: unknown): Promise<Response> {
    if (env) {
      (globalThis as Record<string, unknown>).__OJ_CF_ENV = env;
      (globalThis as Record<string, unknown>).__OJ_CF_CTX = ctx;
    }
    return await handler(request);
  },
};

// The framework's default server entry also exports `createServerEntry`, and
// an app server entry wraps the default through it (`import handler,
// { createServerEntry } from "@tanstack/solid-start/server-entry"`). This
// runner shadows that subpath, so the shadow keeps the named export too:
// the framework's own factory when the installed version provides it on the
// `server` subpath (newer releases re-export it there), else the
// pass-through the older default entries defined locally. Read at call time,
// so a future dependency cycle still links.
type ServerEntryLike = {
  fetch: (request: Request, ...args: Array<unknown>) => Promise<Response> | Response;
};

function fallbackServerEntry(entry: ServerEntryLike): ServerEntryLike {
  return {
    async fetch(...args: Array<unknown>) {
      return await entry.fetch(args[0] as Request, ...args.slice(1));
    },
  };
}

export function createServerEntry(entry: ServerEntryLike, ...rest: Array<unknown>): ServerEntryLike {
  const runtime = serverRuntime as unknown as {
    createServerEntry?: (entry: ServerEntryLike, ...rest: Array<unknown>) => ServerEntryLike;
  };
  const factory = typeof runtime.createServerEntry === "function" ? runtime.createServerEntry : fallbackServerEntry;
  return factory(entry, ...rest);
}
