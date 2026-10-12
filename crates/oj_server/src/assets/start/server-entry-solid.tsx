import { createStartHandler, defaultStreamHandler } from "@tanstack/solid-start/server";
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
