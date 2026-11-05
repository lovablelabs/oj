import { createServerOnlyFn } from "@tanstack/react-start";
export function m(){ const x = createServerOnlyFn(/*<*/() => { const a = 2; return a }/*>*/); const a = g(); return x }
