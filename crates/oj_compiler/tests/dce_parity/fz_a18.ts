import { createServerOnlyFn } from "@tanstack/react-start";
export function m(p = (() => { const a = g(); return createServerOnlyFn(/*<*/() => a/*>*/) })()) { return p }
export const n = () => { const b = g(); return createServerOnlyFn(() => b) };
