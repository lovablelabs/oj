import { createMiddleware, createServerFn } from "@tanstack/react-start";

const logged = createMiddleware({ type: "function" }).server(async ({ next }) => {
  const result = await next();
  return result;
});

export const greet = createServerFn({ method: "GET" })
  .middleware([logged])
  .inputValidator((name: unknown) => {
    if (typeof name !== "string") throw new Error("a name");
    return name;
  })
  .handler(async ({ data }) => ({ text: `hello ${data}` }));

export const count = createServerFn({ method: "POST" })
  .inputValidator((n: number) => n)
  .handler(async ({ data }) => data + 1);
