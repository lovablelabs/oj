import { createServerOnlyFn } from "@tanstack/react-start";
if (globalThis.x) {}
export const fn = createServerOnlyFn(undefined);
