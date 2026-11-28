import { createClientOnlyFn, createIsomorphicFn, createServerOnlyFn } from "@tanstack/react-start";

export const side = createIsomorphicFn()
  .server(() => "server")
  .client(() => "client");

export const secret = createServerOnlyFn(() => process.env.APP_SECRET ?? "none");

export const viewport = createClientOnlyFn(() => window.innerWidth);

export const now = createIsomorphicFn().client(() => Date.now());
