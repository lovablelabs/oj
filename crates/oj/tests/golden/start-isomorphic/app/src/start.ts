import { createMiddleware, createStart } from "@tanstack/react-start";

const timed = createMiddleware({ type: "request" }).server(async ({ next }) => {
  const started = Date.now();
  const response = await next();
  response.headers.set("x-took", String(Date.now() - started));
  return response;
});

export const startInstance = createStart(() => ({ requestMiddleware: [timed] }));
