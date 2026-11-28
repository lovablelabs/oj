import { createIsomorphicFn, createServerOnlyFn } from "@tanstack/react-start";

export const where = createIsomorphicFn()
  .server(() => "server")
  .client(() => "client");

export const secret = createServerOnlyFn(() => process.env.SECRET ?? "none");
