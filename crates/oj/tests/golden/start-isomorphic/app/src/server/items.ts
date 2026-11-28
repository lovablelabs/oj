import { createMiddleware, createServerFn } from "@tanstack/react-start";
import { z } from "zod";

const counted = createMiddleware({ type: "function" })
  .client(async ({ next }) => next({ sendContext: { from: "client" } }))
  .server(async ({ next, context }) => next({ context: { seen: context.from } }));

export const listItems = createServerFn({ method: "GET" })
  .middleware([counted])
  .inputValidator(z.object({ page: z.number().int().min(1) }))
  .handler(async ({ data, context }) => ({
    page: data.page,
    seen: context.seen,
    items: Array.from({ length: 3 }, (_, i) => `item ${(data.page - 1) * 3 + i + 1}`),
  }));

export const rename = createServerFn({ method: "POST" })
  .inputValidator(z.object({ id: z.number(), name: z.string().min(1) }))
  .handler(async ({ data }) => ({ ...data, saved: true }));
